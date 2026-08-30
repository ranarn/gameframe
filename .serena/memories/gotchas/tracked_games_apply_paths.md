## Tracked-games model and apply-path rewiring (issue #31)

Replaces the old `win32::active_preset()` "first preset in the list" MVP hack
(issue #6) with per-game preset assignment. Relevant for future issues #32
(auto-apply on launch) and #33 (live re-apply on edit), both of which build
directly on this model.

### Data model (`src/profile_store.rs`)
- `PresetKind` enum (`Left`/`Right`/`HalfLeft`/`HalfRight`/`Custom`) -- a
  *reference* to one of the five preset kinds, never a copy of preset data.
  `PresetKind::ALL` is the fixed presentation-order array (used by the
  Settings dropdown).
- `Presets::resolve(&self, kind: PresetKind) -> Option<Preset>` resolves a
  `PresetKind` against the live `Presets` set at apply-time. Returns `None`
  **only** for `PresetKind::Custom` when `self.custom` is currently `None`
  (e.g. a game was assigned Custom, then Custom was cleared/replaced) --
  every other kind always resolves. Callers must treat that `None` exactly
  like "untracked game": no-op + log, never silently substitute a different
  preset.
- `TrackedGame { exe_path: PathBuf, display_name: String, preset: PresetKind,
  auto_apply: bool, taskbar_autohide: bool }`. Identity is `exe_path` alone.
  `TrackedGame::new_default(exe_path, display_name)` builds this issue's
  specified defaults: preset `Right`, `auto_apply: true`,
  `taskbar_autohide: true`.
- `upsert_tracked_game(games: &mut Vec<TrackedGame>, exe_path, display_name)
  -> usize` -- pure dedupe-by-exe-path upsert (find existing index, or push a
  `new_default` and return its index). Reusing an existing entry does **not**
  reset its preset/auto_apply/taskbar_autohide, and does **not** overwrite
  `display_name` (captured only at add-time, per `CONTEXT.md`'s "Tracked
  game"/`display_name` doc comment).
- `find_tracked_game(games: &[TrackedGame], exe_path: &Path) ->
  Option<&TrackedGame>` -- the read-only lookup side, used by the hotkey path
  (which never adds, only looks up).
- `Config.tracked_games: Vec<TrackedGame>` (`#[serde(default)]` so old
  configs without the field still deserialize -- though ADR 0001 already
  means a schema mismatch resets to defaults anyway; `#[serde(default)]` here
  is a belt-and-suspenders convenience for a Vec being appended to a
  pre-existing shape without forcing a full reset).

### OS-facing resolution helpers
- `window_filter::exe_path_for_window(hwnd) -> Option<PathBuf>` (new,
  `pub(crate)`) -- `GetWindowThreadProcessId` + `process_exe_path`, shared by
  both the hotkey path (via `win32::foreground_window_exe_path`) and the tray
  "Apply to Window" path (explicit `hwnd`).
- `win32::foreground_window_exe_path() -> Option<PathBuf>` (new) --
  `GetForegroundWindow()` + `window_filter::exe_path_for_window`. This is a
  **second, separate** `GetForegroundWindow()` call from the one
  `toggle_borderless_on_foreground_window` makes internally (that function's
  signature was deliberately left unchanged -- it still resolves its own
  `HWND` and never surfaces it to the caller). The two calls are expected to
  agree in practice; see the doc comment on `foreground_window_exe_path` for
  why a race here is low-risk.
- `win32::set_taskbar_autohide_absolute(auto_hide: bool) -> Result<(),
  Win32Error>` (new, `pub`) -- wraps the existing private `set_appbar_state`
  via a pure `taskbar_autohide_state_from_bool` conversion (unit tested).
  Fully independent of `toggle_taskbar_autohide`/the global hotkey's own
  `TaskbarAutohideState` toggle -- per ADR 0002, that mechanic stays
  untouched.
- `win32::window_text` is now `pub(crate)` (was private) -- used by
  `app::handle_apply_to_window` to capture a new tracked game's
  `display_name` at add-time.
- `win32::active_preset` and its test were **deleted** (see ADR
  0001's consequence note -- this was always where it was meant to land).

### Apply-path rewiring (`src/app.rs`)
- New pure helper `resolve_tracked_apply(config: &Config, exe_path: &Path) ->
  Option<(TrackedGame, Preset)>` -- the shared "look up tracked game, resolve
  its preset" decision behind both apply paths. Returns `None` for both
  "untracked" and "tracked but Custom-cleared" -- fully unit tested in
  `app::tests` (three cases: untracked, resolves fine, Custom-cleared).
- `handle_toggle_borderless` (hotkey path): resolves foreground exe path →
  `resolve_tracked_apply` (find-only, never upserts) → if `None` **and this is
  a strip press**, log and return, no fallback (see the `stripped_hwnd`
  section below for why a pending restore is never gated on tracked-game
  membership) → else apply border/region via
  `toggle_borderless_on_foreground_window`, then, **on the strip direction
  only**, call `set_taskbar_autohide_absolute` with the tracked game's field
  (on success of the border step). Taskbar-set is deliberately *not* fired on
  the restore/undo direction (see judgment call #2 below, revised in
  iteration 3) -- firing it again on restore, with the same fixed per-game
  boolean, would leave the taskbar auto-hidden after a full strip+restore
  cycle instead of it coming back, for the common `taskbar_autohide: true`
  default.
- `handle_apply_to_window` (tray path): resolves target `hwnd`'s exe path →
  `upsert_tracked_game` (adds with defaults if new, reuses if not) →
  **saves the config immediately** (before attempting border/region/taskbar
  steps) so the tracked-list mutation survives even if a later step fails →
  resolves preset via `config.presets.resolve` → applies border via
  `apply_borderless_to_window` (early return, no taskbar-set, on failure) →
  sets taskbar absolute unconditionally on success. This is a one-shot path
  with no toggle/restore concept, so it always sets taskbar-autohide on a
  successful apply -- unlike the hotkey path, there's no "restore direction"
  to gate here.

### `stripped_hwnd` / restore-safety mechanism (hotkey path only, iterations 3-4)
`toggle_state: Option<RestoreState>` (owned by `run_event_loop`, threaded into
`handle_toggle_borderless` as `&mut`) already tracks *whether* a restore is
pending. It does **not** track *which window* it's pending for. A second,
parallel `stripped_hwnd: Option<HWND>` local was added alongside it to close
that gap:

- **Why it exists (iteration 3 HIGH)**: without it, alt-tabbing away from a
  freshly-stripped game to any other window (Chrome, Explorer, another
  tracked game) and pressing the hotkey again would blindly apply the
  *original* game's saved style/region to *whatever window currently has
  focus* -- corrupting that unrelated window while permanently orphaning the
  original game's window in its stripped state.
- **`should_allow_restore(current: Option<HWND>, stripped: Option<HWND>) ->
  bool`** (`app.rs`, pure, unit tested) -- `current.is_some() && current ==
  stripped`. `handle_toggle_borderless` consults this *before* calling
  `toggle_borderless_on_foreground_window` whenever `is_restoring` is true; a
  mismatch (or no foreground window) logs and returns `false` **without**
  touching `toggle_state`/`stripped_hwnd`, so the user can still complete the
  correct restore later by refocusing the originally-stripped window.
- **Commit-early timing (iteration 4 HIGH, the most recent fix)**: `win32.rs`'s
  own `MakeBorderless` arm commits `toggle_state = next_state` *before*
  attempting the OS-mutating calls, specifically so a partial failure mid-call
  doesn't lose track of the remembered state (see that arm's doc comment).
  `stripped_hwnd` must mirror this same timing for the strip direction, or a
  partial failure leaves `toggle_state` committed (by `win32.rs`, as designed)
  but `stripped_hwnd` still `None` -- a desync where the *next* press reads
  `is_restoring = true` but `should_allow_restore` refuses it forever, since
  `stripped_hwnd` was never recorded. Fixed by setting `*stripped_hwnd =
  fg_hwnd` in `handle_toggle_borderless` **immediately before** calling
  `toggle_borderless_on_foreground_window`, guarded by `if !is_restoring`
  (mirroring, not duplicating, `win32.rs`'s own early-commit rationale). The
  restore direction is *not* committed early -- `*stripped_hwnd = None` is
  only set *after* a successful call, mirroring `win32.rs`'s own `Restore` arm
  (which only clears `toggle_state` after all three restore calls succeed):
  a failed restore must still leave `stripped_hwnd` in place so a retry on the
  same window can succeed.
- This mechanism is specific to the **hotkey** path. `handle_apply_to_window`
  (tray path) has no toggle/restore concept at all -- see this module's own
  doc comment -- so it needs no equivalent.
- **Relevant for #32/#33**: both future issues build on this model. #32
  (auto-launch detection) will need its own decision about whether a
  programmatically-triggered apply should interact with `stripped_hwnd` at
  all (the current mechanism is scoped to manual hotkey presses only). #33
  (live re-apply on edit) should be aware that editing a tracked game's
  preset/assignment while its window is mid-restore (`toggle_state.is_some()`)
  does not currently invalidate or reconcile `stripped_hwnd` -- undocumented
  prior to this note.

### Judgment calls made (no user/orchestrator to confirm with -- AFK mode)
1. **Untracked foreground window during a restore press** -- REVISED during
   review-fix pass (MEDIUM finding): originally gated *both* directions
   uniformly (see git history for the superseded rationale), but that
   permanently orphans a currently-stripped window if its tracked game is
   removed via Settings (or its `Custom` preset cleared) between the strip
   press and the restore press -- nothing else in the app can restore it
   once that happens. Fixed via a new pure `should_block_untracked_toggle(is_restoring,
   tracked) -> bool` (`!is_restoring && !tracked`) in `app.rs`, unit tested
   for all four combinations. `handle_toggle_borderless` now computes
   `is_restoring = toggle_state.is_some()` *before* calling
   `toggle_borderless_on_foreground_window` (this mirrors `plan_toggle`'s own
   `remembered` check, since `toggle_state` **is** `plan_toggle`'s
   `remembered` argument) and only gates the strip direction. When
   restoring with no resolved tracked game, `target_region` is filled with
   an inert placeholder (`Region { x: 0, y: 0, width: 0, height: 0 }`) since
   `plan_toggle`'s `Restore` arm never reads it, and the taskbar-autohide-set
   step is skipped (no tracked game to read the flag from).
2. **Taskbar-autohide set on strip vs. restore** -- REVISED during review-fix
   pass (MEDIUM finding, iteration 3): originally fired
   `set_taskbar_autohide_absolute` unconditionally after *any* successful
   `toggle_borderless_on_foreground_window` call when a tracked game was
   resolved (both directions), reasoning that this matched the issue brief's
   literal linear pipeline description over a stricter reading of ADR 0002.
   That was wrong in practice: for the common `taskbar_autohide: true`
   default, firing the same fixed per-game boolean on *both* directions means
   the taskbar never returns to visible after a full strip+restore cycle --
   it reads as a bug to a real user exiting a game. **Current behavior**: the
   taskbar-set call is gated on `!is_restoring` -- it only fires on the strip
   direction. Restoring undoes the border/region change but deliberately
   leaves the taskbar alone (the user's own separate global taskbar-autohide
   hotkey, or whatever state it was already in, governs it from there). The
   tray path (`handle_apply_to_window`) is unaffected by this -- it has no
   restore concept, so it always sets taskbar-autohide on a successful apply.
3. **`Config.tracked_games` gets `#[serde(default)]`** even though ADR 0001
   means a schema mismatch already resets to defaults -- purely defensive,
   costs nothing. Regression-tested directly (review MEDIUM finding): see
   `load_of_a_current_shape_config_file_missing_only_the_tracked_games_key_still_deserializes`
   in `profile_store::tests`, distinct from the pre-existing
   `load_of_an_old_shape_config_file_resets_to_defaults_with_no_migration`
   test (that one covers a pre-#26 shape that doesn't deserialize at all;
   this one covers a valid post-#26 shape simply missing the newer key).
4. **`handle_apply_to_window`'s taskbar-set is now gated on the border-apply's
   success** (review HIGH finding) -- previously fell through
   unconditionally even when `apply_borderless_to_window` failed (e.g.
   `Win32Error::WindowMinimized`), forcing taskbar auto-hide for a window
   that was never actually touched. Now mirrors `handle_toggle_borderless`'s
   early-return-on-failure shape.
5. **`PathBuf` `Eq` case-sensitivity** on `upsert_tracked_game`/
   `find_tracked_game` (review LOW finding) -- documented as an accepted
   risk in both functions' doc comments rather than fixed: both apply paths
   resolve `exe_path` via the same OS API for a live process, so
   differently-cased paths for the same game aren't expected in practice.

### Settings UI (`src/settings_ui.rs`)
- `PresetThumbnail`/`preset_thumbnails` **lost their `is_active` field**
  entirely (was driven by the now-deleted `win32::active_preset`) -- a
  preset's "active" status is per-tracked-game now, not global, so it had no
  meaning left in the shared five-preset thumbnail row. The two tests that
  asserted on `is_active` were removed/updated accordingly.
- `SettingsState.tracked_games: Vec<TrackedGame>` carried through
  `from_config`/`to_config` like every other field.
- `SettingsState::remove_tracked_game(&mut self, index)` -- immediate,
  no-confirmation removal (mirrors `clear_hotkey`'s pattern).
- New "Games" section in `SettingsApp::ui` (between "Global hotkeys" and the
  Save/Cancel row): one row per tracked game, `egui::ComboBox` preset
  dropdown driven by `PresetKind::ALL`/`preset_kind_label`, two checkboxes,
  a Remove button, and a `ui.separator()` between rows (review LOW finding).
  UI rendering itself is NOT unit tested (egui/OS-facing, same convention as
  the rest of this module) -- only the `SettingsState` mutation methods and
  `preset_kind_label` are.
- `preset_kind_unavailable_reason(kind, presets, monitor) -> Option<String>`
  (review MEDIUM finding) -- pure, unit-tested helper mirroring
  `preset_thumbnails`'s per-preset error path but keyed off a `PresetKind`
  reference: `None` via `presets.resolve` (unassigned/cleared `Custom`) or a
  `resolve_region` failure (e.g. `Custom` coordinates don't fit the current
  monitor) both surface as a red "Unavailable: ..." label next to that row's
  preset dropdown in `games_section`, consistent with the thumbnail row's
  existing error-surfacing pattern.

See `mem:gotchas/window_filter_module_split` for the pre-existing
pure/OS-facing split convention this issue followed throughout.
