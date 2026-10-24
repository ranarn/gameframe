# Road to GameFrame 1.0 — release, auto-update and readiness review

Status: research, 2026-10-23, at commit `36e9b4c`. Nothing here is implemented yet.

Goal: GameFrame 1.0 can be downloaded from GitHub Releases, follows semver, and
notices newer releases itself — installing them either at startup or from a tray
item that appears when an update is available.

Baseline at `36e9b4c`: `cargo test` passes (278 passed, 6 ignored, OS-facing),
`cargo clippy --all-targets` reports 17 warnings, release exe 9.2 MB, installer
(Inno Setup 7) 5.0 MB. No tags, no releases, no CI, no LICENSE. The repo is
private today and goes public, open source under GPL-3.0, before 1.0.

---

## 1. Decisions (taken 2026-10-23)

| # | Decision | Outcome |
|---|----------|---------|
| P1 | Where releases are published | **`ranarn/gameframe` goes public, open source**; releases are published in the same repo. An installed app can't read a private repo's releases without shipping a token, while public release assets and `GET /repos/{o}/{r}/releases/latest` need no auth (60 unauthenticated requests/hour per IP — far more than one check per start). Public also makes Actions minutes free and enables branch protection. |
| P2 | License | **GPL-3.0-or-later** (`LICENSE` = the official text from gnu.org). Every dependency is under a permissive license (MIT, Apache-2.0, BSD, Zlib, ISC, BSL-1.0, Unicode-3.0, OFL-1.1/Ubuntu-font for egui's bundled fonts — checked via `cargo metadata`, 2026-10-24), all GPL-3.0-compatible. Their notices must ship with the binary (§5 M7). |
| P2b | Funding | **Voluntary, via PayPal** (`https://www.paypal.com/donate/?hosted_button_id=YPDPUVMSDQLJQ`). GameFrame is free with no paywalled features; the link is in `.github/FUNDING.yml`, the README, and later Settings > About (§4). |
| P2c | Public footprint | The commit email stays as is. `CLAUDE.md`, `.claude/` and `.serena/` are untracked and gitignored from now on; their earlier versions stay in the history and in old PR diffs (accepted — a scan found nothing secret). Issues and PR texts become public (accepted). |
| P3 | Updater approach | **Option A** — Inno Setup + a small built-in updater (§3). |
| P4 | Update UX | **Both, as a setting** — install automatically at start (default) or "Ask me" via a tray item (§4). |
| — | Open bug #52 | **Ships as a known issue** in 1.0, fixed in a 1.0.x patch. |

---

## 2. Versioning and release pipeline

### Semver policy for an app

GameFrame has no library API, so the "public API" semver protects is:

- **`config.toml`** (shape and meaning of every key) and the `%APPDATA%\GameFrame` layout,
- the **`HKCU\...\Run` value `GameFrame`** and the installer's `AppId`,
- user-visible behavior of hotkeys, tray menu and auto-apply.

| Bump | When |
|------|------|
| MAJOR | A config change that older/newer versions can't read without migration, a removed feature, a changed default that alters behavior for existing users. |
| MINOR | New features, new config keys with `#[serde(default)]`. |
| PATCH | Bug fixes only. |

The existing conventional-commit prefixes map onto this (`feat:` → minor,
`fix:` → patch, `feat!:`/`BREAKING CHANGE:` → major).

### Single source of truth

`Cargo.toml` `version` → already flows into the exe's VERSIONINFO (verified:
FileVersion/ProductVersion = `0.1.0`) and into the installer via `build.ps1`.
The git tag is `v<version>`; CI refuses to release if the tag and `Cargo.toml` disagree.

### Proposed release flow (tag-driven)

1. Bump `version` in `Cargo.toml` (+ `Cargo.lock`), update `CHANGELOG.md`, commit `chore: release vX.Y.Z`.
2. `git tag vX.Y.Z && git push --tags`.
3. GitHub Actions (`.github/workflows/release.yml`, `windows-latest`):
   `cargo test` → `cargo clippy -- -D warnings` → `installer\build.ps1` →
   (optional) sign → `gh release create vX.Y.Z` with `GameFrame-Setup-X.Y.Z.exe`
   attached and the `CHANGELOG.md` section as notes, using the workflow's own
   `GITHUB_TOKEN` (`permissions: contents: write`).
4. A tag with a pre-release suffix (`v1.1.0-beta.1`) is published as a GitHub
   *pre-release*, which `releases/latest` ignores — so beta builds never reach the updater.

Plus a `ci.yml` running test + clippy on every PR.

Optional later: `release-please` to automate steps 1–2 from conventional commits.
Not needed for 1.0.

Note: the CI runner needs Inno Setup 7 installed in the job (the runner image's
preinstalled Inno version is to be verified; `build.ps1` only accepts 7).

---

## 3. Updater approach (decision P3)

### Option A — keep Inno Setup, add a small built-in updater (recommended)

Flow:

1. Background thread, ~30 s after start (and every 24 h while running):
   `GET https://api.github.com/repos/ranarn/gameframe/releases/latest`.
2. Parse `tag_name`, compare with `CARGO_PKG_VERSION` as semver (`semver` crate or ~20 lines).
3. Pick the asset named `GameFrame-Setup-*.exe`. Each asset carries a
   `digest: "sha256:<hex>"` field (verified on a live `releases/latest`
   payload, `cli/cli` v2.103.0, 2026-10-23) — use it to verify the download.
4. Download to `%TEMP%\GameFrame-update\`, verify SHA-256.
5. Install: **GameFrame first shuts down gracefully** (the same path as tray "Exit":
   restores an armed taskbar hide, stops the cursor thread), launching
   `Setup.exe /VERYSILENT /SUPPRESSMSGBOXES /NORESTART` just before it exits.
   The installer waits for the single-instance mutex to disappear
   (`CheckForMutexes` loop in `PrepareToInstall`) before falling back to `taskkill /F`,
   and relaunches GameFrame afterwards (`[Run]` entry with `skipifnotsilent`).

Needs: an HTTPS client (`ureq` with rustls, or WinHTTP through `windows-sys`
with no new crate), SHA-256 (`sha2`, or BCrypt via `windows-sys`), a JSON parser
(`serde_json` — serde is already in).

| + | − |
|---|---|
| Keeps the working installer, its `AppId` upgrade path, the "Start with Windows" task and per-user install. | Full installer per update (~5 MB) — irrelevant at this size. |
| Small, fully owned code (~300 lines + tests). | Update UX, retries, error handling are ours to write. |
| No new toolchain in the build. | |

### Option B — Velopack

Velopack (Rust crate `velopack` 1.2.161, 2026-09-29) replaces Inno Setup:
`vpk pack` builds a per-user `Setup.exe` (installs to `%LocalAppData%\{packId}`),
`vpk upload github` publishes, and the app calls
`UpdateManager::new(GithubSource…)` → `check_for_updates` → `download_updates` →
`apply_updates_and_restart`. `VelopackApp::build().run()` must be the first call in `main`.

| + | − |
|---|---|
| Delta updates, staged apply, mature framework. | Throws away the Inno installer and its upgrade logic; the 0.x install would need a manual uninstall. |
| GitHub source built in. | Adds the .NET `vpk` tool to the build, and a larger dependency tree to a size-tuned binary. |
| | Its own install layout and shortcuts; the "Start with Windows" checkbox moves into hooks or the app. |
| | Still needs the same graceful-shutdown work (taskbar, cursor) before `apply_updates_and_restart`. |

### Rejected

- **`self_update` crate** — swaps the exe in place; the installer's version in
  Settings > Apps goes stale and the uninstaller drifts.
- **cargo-dist / axoupdater** — built around its own shell/MSI installers; would
  replace Inno with no gain over A.

**Recommendation: A.** The app is a single 9 MB exe with a working installer;
Velopack's strengths (deltas, multi-file apps, cross-platform) don't pay for
themselves here.

---

## 4. Update UX (decision P4)

Proposal:

- **Settings > General > "Updates"**: version shown, a "Check now" button, and a
  choice: *Install updates automatically at startup* (default) / *Ask me (tray)*.
- **Automatic**: on start, if an update is found it installs right away, before
  any game is tracked — the startup is the one moment nothing is in use. Never
  mid-session.
- **Ask me**: a tray item **"Update to vX.Y.Z"** appears when an update is
  available (plus a one-time tray balloon). Clicking it runs step 5.
- Never update while a Zone apply has the taskbar hidden, or while Settings or
  the zone editor is open — defer to the next idle point.
- No network or GitHub down → log a single line, try again next start. Never a dialog.
- A **"Release notes"** link from the GitHub release body in Settings.

### About and voluntary support

- **Settings > About** (new tab, or a section under General): version, license
  (GPL-3.0), links to the GitHub repo, issues ("Report a bug" — points at the log
  file location), third-party licenses, and **"Support GameFrame"** → the PayPal link.
- **Tray**: an "About / Support GameFrame" item is optional; never a nag, a
  popup or a reminder — support stays voluntary and out of the way.
- The installer's last page and the README carry the same link once.

---

## 5. Code review findings

Severity: **High** = must fix before 1.0, **Medium** = should fix, **Low** = cleanup.
"Confirmed" = shown by running code or reading the exact code path end to end;
"Plausible" = from reading the code, not reproduced.

### High

**H1. Elevated relaunch kills GameFrame instead of elevating it** — plausible.
`app::handle_toggle_borderless` → `win32::relaunch_elevated()` starts an
elevated copy and returns `true`, and the current instance exits. But the new
copy hits `single_instance::acquire()` while the old instance still holds the
`Local\GameFrame-…` mutex (single instance landed in `e36ad7f`, after the
relaunch path), so the elevated copy exits at once → no GameFrame running at all.
*Verify*: run a game as administrator, bind the borderless hotkey, press it on
the game → tray icon disappears, no `gameframe.exe` in Task Manager.
*Fix*: pass a `--elevated-relaunch` argument and let that instance wait for
the mutex for a few seconds, or release the mutex before `ShellExecuteW`.

**H2. A killed or crashed GameFrame leaves the taskbar hidden** — confirmed (code path).
There is no panic hook, and the installer's `taskkill /F` (every upgrade and
uninstall) skips all cleanup. `FullyHidden` (`ShowWindow(SW_HIDE)`) lasts until
Explorer restarts; `HoverReveal` (`ABS_AUTOHIDE`) is **persistent** — the user's
taskbar stays auto-hide for good. Auto-update makes this frequent.
The zone editor's `TaskbarRestoreGuard` doc claims a panic unwinding through
`eframe::run_native` restores the taskbar, but the release profile is
`panic = "abort"`, so no `Drop` runs on a panic in a shipped build.
*Fix*: graceful shutdown before installing (§3 step 5); a `std::panic::set_hook`
that logs the panic to `gameframe.log` (today panics vanish: no console,
`panic = "abort"`) and restores an armed taskbar hide; optionally handle
`WM_ENDSESSION`.

**H3. An upgrade overwrites the user's "Start with Windows" choice** — plausible.
`gameframe.iss` `[Registry]` writes the Run value when the `startup` task is
checked and **deletes** it when it isn't. On an upgrade Inno reuses the previous
task choice (`UsePreviousTasks`, default yes), so: install without the checkbox →
enable "Start with Windows" in the tray → next update silently removes autostart.
(And the other way round.) With auto-update this happens on every release.
*Verify*: install without the task, enable from the tray, run the same installer
again silently, check `HKCU\...\Run`.
*Fix*: apply the `startup` task only on first install (`Check: not IsUpgrade`)
and leave the registry alone on upgrades — the app owns that value afterwards.

### Medium

**M1. DPI awareness depends on whether an egui window has opened yet** — plausible.
No app manifest and no process-wide `SetProcessDpiAwarenessContext`; only the
cursor thread sets per-thread awareness. `primary_monitor_geometry()`
(`GetSystemMetrics`) and every `SetWindowPos` therefore run DPI-unaware (scaled
values on a >100 % display) until winit makes the process DPI-aware when Settings
or the zone editor first opens. The Zone could land differently before and after
the first Settings visit on a scaled display.
*Verify*: 150 % scaling, start GameFrame, apply via hotkey; open and close
Settings, apply again; compare positions.
*Fix*: embed a manifest (`winresource` `set_manifest`) with
`PerMonitorV2` dpiAwareness — also gives Common Controls v6.

**M2. Open bug #52** (taskbar hotkey stops working after long uptime) — an
update-at-startup design makes long uptimes rarer but doesn't fix it. Decided:
ships as a known issue (listed in the release notes), fixed in 1.0.x.

**M3. Second launch is silent.** Starting GameFrame again (Start menu, desktop
icon) while it runs does nothing visible. For 1.0, signal the running instance
to open Settings (a registered window message or a named event).

**M4. Clippy: 17 warnings** (7 collapsible `if`, 2 `chunks_exact`, 1
`sort_by_key`, 1 too-many-arguments, 1 complex type, …). Fix, then gate CI with
`-D warnings`.

**M5. Drift re-apply could ping-pong** — plausible, low likelihood.
`EVENT_OBJECT_LOCATIONCHANGE` → `handle_applied_window_drift` re-applies
whenever an applied window moves. A game that insists on its own size would
loop `SetWindowPos` ↔ the game, and every event reloads `config.toml` from disk.
*Fix*: rate-limit re-applies per window (e.g. at most N within 2 s, then give up and log).

**M6. Exe metadata.** `ProductName` and `FileDescription` are `gameframe`
(Task Manager and the SmartScreen dialog show the latter); `CompanyName` and
`LegalCopyright` are empty; `Cargo.toml` has no `description`, `license`
(`GPL-3.0-…`), `repository`, `authors`.

**M7. Third-party license notices.** The binary statically links ~430 crates
(plus egui's bundled fonts under OFL-1.1/Ubuntu-font). MIT/Apache/BSD require
their notices to travel with the binary. Generate `THIRD-PARTY-LICENSES.html`
in CI (e.g. `cargo about generate`), install it next to the exe, and link it
from Settings > About. Add SPDX headers or at least the GPL notice to the README.

### Low — old comments and dead code ("onödig gammal kod")

**L1. ~135 history comments.** Doc comments narrate how the code came to be
instead of what it does: "HIGH review finding, issue #31 iteration 3", "this
issue's acceptance criteria", "for this MVP", "out of scope for this slice",
"pre-#42 behavior", "Feature-flag correction" (a whole module-doc section in
`launch_watcher.rs` about a `Cargo.toml` feature found during an issue).
Per file (grep for `issue #N|iteration N|review finding|this slice|this issue|MVP|preset`):
`profile_store` 31, `win32` 29, `app` 26, `settings_ui` 26, `launch_watcher` 9,
`zone_overlay` 5, `window_filter` 3, `keyboard_hook` 3, others 1 each.
Rule for the clean-up: keep the *why* (invariant, OS quirk, rejected
alternative), drop the *who/when* (issue numbers, iterations, review severities).
Git history and the ADRs hold the provenance.

**L2. Comments that are now wrong:**
- `app.rs:100-101` — "restores swapped system cursors": nothing calls
  `SetSystemCursor` anywhere; the claim is false.
- `tray_ui.rs` module doc, `Tray::new`, `TrayMenuIds` docs, a test name —
  "three top-level items"; there are four ("Start with Windows").
- `settings_ui.rs` module doc and `HotkeyField` — "two hotkey bindings/fields"; there are three.
- `app.rs` module doc — lists only two hotkeys.
- `tray_ui.rs:134` — "placeholder tray icon image"; it's the real icon.
- `settings_ui::primary_monitor_geometry` — "MVP scope".
- `win32.rs:30-31` — "See the module's doc comment in the issue writeup".

**L3. Dead code:** `win32::foreground_window_exe_path` (`#[allow(dead_code)]`,
documented as having no caller) — delete.

**L4. Legacy config migration.** `profile_store`'s preset-era conversion
(`LegacyConfig`, `LegacyPresets`, `convert_legacy_config`,
`keep_pre_zone_backup`, `fold_per_game_taskbar_flags` and ~15 tests) exists only
to migrate pre-release configs. The only install (this machine) is already
migrated (`config.toml.pre-zone.bak` from 2026-10-22). Remove for 1.0, if no other
machine still runs a 0.x preset-era build.

**L5. Throwaway prototypes in `examples/`:** `zone_overlay_prototype/` (+
`NOTES.md`) and `zone_overlay_visual/` are labeled "PROTOTYPE — throwaway" and
were superseded by `src/zone_editor.rs`. Delete; this also drops the `log`
dev-dependency. Keep `examples/gen_icon.rs` (regenerates `assets/icon.ico`).

**L6. README is a dev log.** "Status: MVP complete", internal memory
measurements, a link to `../Project Ideas/gameframe.md` outside the repo
(broken for anyone else), "binding the two global hotkeys". A 1.0 README needs:
what it does, screenshots, install (with the SmartScreen note), usage, where
config/log live, uninstall, known limitations (primary monitor only; Windows
10/11 x64), the no-injection / anti-cheat stance, and build-from-source.

**L7. Stale Serena memories:** `core` (describes the issue #4 tray shell),
`tech_stack` ("still-planned: egui, global-hotkey"), `suggested_commands`
("no tests exist yet"), `conventions` ("no CONTEXT.md yet"). Rewrite or trim.

**L8. Config forward-compatibility.** An unknown enum value (e.g. a cursor shape
added in 1.1) makes an older build reset the *whole* config to defaults (with a
`.bak`). Rare (needs a downgrade), but worth a note in the semver policy: new
enum variants are a MINOR change that downgrade can't read.

### What's already in good shape

Atomic config save (temp + rename), backup before reset, log rotation, the
per-user installer with a fixed `AppId`, the registry as the single source of
truth for autostart, single instance, static CRT (no VC++ redist), version
flowing from `Cargo.toml`, and a solid unit-test layer around the pure logic.

---

## 6. Proposed 1.0 work, in order

0. **Public-repo readiness — done** on branch `chore/prepare-public-repo`:
   agent tooling untracked and gitignored; local paths and personal references
   removed (docs.rs links instead of `file:///C:/Users/...`); prototypes in
   `examples/` deleted (L5); `LICENSE` (GPL-3.0-or-later), `Cargo.toml`, exe and
   installer metadata (M6); `SECURITY.md`, `.github/FUNDING.yml`,
   `dependabot.yml`, bug report template; `cargo audit` clean (tokio
   RUSTSEC-2026-0384 bumped, yanked clipboard-win bumped; 4 "unmaintained"
   warnings remain — `ttf-parser` via egui, and Linux-only `zbus` crates); clippy
   clean (M4) and `ci.yml` gating clippy `-D warnings`, tests and audit; README
   Support and License sections. Once merged, the repo is safe to make public;
   the rest of the 1.0 work can happen in the open.
1. **Hygiene** — L1–L4, L7: history comments, wrong comments, dead code, legacy migration, Serena memories. No behavior change.
2. **Robustness** — H1, H2 (panic hook + graceful shutdown), M1 (manifest), M3, M5.
3. **Installer** — H3, wait for the mutex before `taskkill`, `[Run]` relaunch after a silent update.
4. **Open-source packaging** — third-party notices (M7), `CONTRIBUTING.md` (issues welcome, how to build).
5. **Release pipeline** — `release.yml`, `CHANGELOG.md`, version 1.0.0-rc.1 as a pre-release.
6. **Updater + About/Support** — §3 option A plus §4, tested end to end from `1.0.0-rc.1` → `1.0.0-rc.2` (pre-releases, invisible to `releases/latest`, so the test points the updater at the full release list or a test repo).
7. **Docs** — README rewrite (L6), SmartScreen and AV note.
8. **Release 1.0.0.**

---

## 7. Manual steps for the owner

Already decided: GPL-3.0-or-later, the PayPal link, the commit email stays,
agent files untracked. What's left for you, in order:

1. **Review and merge the `chore/prepare-public-repo` PR.** The new CI runs on it —
   check that both jobs (`check`, `audit`) go green on GitHub's runners.
2. **Delete the 8 old remote branches** (all squash-merged via PRs #44–#58;
   each can be restored from its PR page):
   `git push origin --delete chore/dependency-updates feat/installer-autostart feature/cursor-highlight feature/post-testing-ux-and-windows-integration-fixes feature/single-zone-replaces-presets fix/drop-gtk-deps fix/game-own-pivot fix/game-own-tip-pivot`
   Then turn on Settings → General → "Automatically delete head branches".
3. **Make the repo public** — `ranarn/gameframe` → Settings → General → Danger
   Zone → Change visibility → Public. Add a description, topics (`windows`,
   `gaming`, `borderless`, `rust`) and the website field.
4. **Security settings** (available once public) — Settings → Code security:
   turn on Private vulnerability reporting (`SECURITY.md` points there), Dependabot
   alerts and security updates, Secret scanning and Push protection.
5. **Actions settings** — Settings → Actions → General: Workflow permissions →
   "Read and write" (for the coming `release.yml`); Fork pull request workflows →
   "Require approval for first-time contributors".
6. **Protect `main`** — Settings → Rules → Rulesets → New branch ruleset for
   `main`: require a pull request, require the status checks `check` and `audit`,
   block force pushes and deletion.
7. **Funding button** — once `.github/FUNDING.yml` is on `main`, GitHub shows a
   "Sponsor" button with the PayPal link (Settings → General → Features →
   "Sponsorships" must be ticked). Donations may count as income — check with
   Skatteverket if the amounts grow.
8. **Code signing (optional, later).** Unsigned, the first download shows
   SmartScreen's "Windows protected your PC" until the file builds reputation.
   Updates GameFrame downloads itself don't carry the Mark-of-the-Web, so they
   don't trigger SmartScreen. Now that it's open source, **SignPath Foundation**
   (free code signing for OSS under an OSI license — GPL-3.0 qualifies) is an
   option; apply once the repo is public and has a release. Alternative: Azure
   Artifact Signing (monthly fee; check eligibility for individuals in Sweden).
9. **Per release**: bump the version, update `CHANGELOG.md`, tag `vX.Y.Z`, push the
   tag, check the release page. (Fully scripted after §6 step 5.)
10. **Smoke test each release on a clean Windows user** (or Windows Sandbox):
    download → SmartScreen → install → draw Zone → apply to a game → update from
    the previous version → uninstall (taskbar visible, autostart gone, config kept).