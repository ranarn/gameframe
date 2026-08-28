GameFrame — Windows system-tray utility: makes a game/app window borderless, repositions it to a
defined screen region, toggles taskbar auto-hide via global hotkey. Replaces "Windowed Borderless
Gaming" + a standalone taskbar-hide tool with one process.

Status: idle tray shell landed (issue #4) on top of the two pure-logic modules (#2, #3).
`src/main.rs` calls `app::run()`, which shows a tray icon (no window) and blocks in a Win32
message loop until "Exit" is selected.

Source map:
- `src/main.rs` — binary entry point; declares `app`, `profile_store`, `region_calc`, `tray_ui`.
  Calls `app::run()`, printing an error and exiting 1 on failure.
- `src/app.rs` — (issue #4) thin orchestrator. `run()` builds `tray_ui::Tray` then calls
  `run_event_loop`, a Win32 `GetMessageW`/`TranslateMessage`/`DispatchMessageW` pump (via
  `windows-sys`) that, after each dispatched message, drains `muda::MenuEvent::receiver()` and
  checks `tray.ids().is_exit(event.id())`. On Exit it returns, dropping `Tray` (and thus the OS
  tray icon + its hidden window) — that Drop is the entire clean-shutdown path. "Show Settings"
  and "Apply to Window" are no-ops in this slice (their menu items are also `enabled: false`, so
  in practice they can't even fire yet) — real behavior lands in later slices. No business logic
  lives in `app` by design (brief: "keep it thin").
- `src/tray_ui.rs` — (issue #4) owns the tray icon + 3-item context menu ("Show Settings", "Apply
  to Window", "Exit") and the id -> action mapping that recognizes "Exit". Split for testability:
  `TrayMenuIds` (fixed string `MenuId`s + `action_for`/`is_exit`) is pure data — no OS calls — and
  is the fully-unit-tested part. `Tray::new()` is OS-facing (creates a real `muda::Menu` +
  `tray_icon::TrayIcon`, i.e. actual Win32 `CreateMenu`/`CreateWindowExW` calls) and is
  deliberately NOT unit tested — per the PRD this module's primary verification is manual usertest
  on real hardware; a headless/CI sandbox can't visually confirm a tray icon renders. `TrayUiError`
  follows the same manual Display/std::error::Error convention as `RegionCalcError`/
  `ProfileStoreError` (no `thiserror` in the codebase, even though `tray-icon`/`muda` themselves
  use `thiserror` internally — that's fine, it's their own crate's business).
- `src/region_calc.rs` — pure arithmetic resolving a `Preset` (LeftHalf/RightHalf/FullScreen/
  Custom) against a `MonitorGeometry` into a `Region`. No OS calls, no I/O. Still has its
  `#![allow(dead_code)]` — no consumer landed yet (out of scope for #4).
- `src/profile_store.rs` — (issue #3) loads/saves GameFrame's config (presets + two named hotkey
  bindings) as TOML. Still has its `#![allow(dead_code)]` — no consumer landed yet (out of scope
  for #4). See prior memory content for full API details (unchanged by #4).
- `README.md` — problem statement summary; full design/research lives outside this repo at
  `Project Ideas/gameframe.md` (sibling directory, not in this repo).
- `.claude/project/` — engineering-pipeline config: `issue-tracker.md` (GitHub Project #4 field
  IDs), `triage-labels.md` (label vocabulary), `domain.md` (how to consume domain docs),
  `project-config.json` (raw board field IDs).
- No `CONTEXT.md` / `docs/adr/` yet (single-context layout is planned per `.claude/project/domain.md`
  but not yet created — don't flag its absence, it's created lazily by `/grill-me`).

See `mem:tech_stack` for dependencies (now includes `tray-icon`/`muda`/`windows-sys`, added in
issue #4), `mem:suggested_commands` for build/run, and `mem:conventions` for repo-specific process
(issue tracker, triage labels).
