Issue #28 added `src/zone_editor.rs` (pure state machine: `Empty -> Dragging -> Placed <-> Resizing`,
`PinnedEdges`/`Handle::enabled`, `AppState::apply(Action) -> Option<SessionOutcome>`) and
`src/zone_overlay.rs` (OS-facing eframe/wgpu transparent overlay window driving it, lifted from
`examples/zone_overlay_prototype/zone_editor.rs` + `examples/zone_overlay_visual/main.rs`).

Key decisions:
- `zone_editor` uses `profile_store::CustomRegion` directly instead of a separate `Zone` type
  (identical fields) so `Save`'s output plugs straight into `Presets.custom`.
- Taskbar force-on/restore-previous lives in `win32.rs` as a pure/OS-facing split, mirroring
  `plan_toggle`/`RestoreState`: `plan_taskbar_force_on(current) -> (to_apply, to_remember)` is pure
  and tested; `force_taskbar_autohide_on()`/`restore_taskbar_autohide()` are the OS-facing wrappers
  (added `read_appbar_flags`/`state_from_flags`/`write_appbar_state` helpers, refactoring
  `set_appbar_state` to share the `ABM_GETSTATE` read rather than duplicate it).
  `force_taskbar_autohide_on` resolves the taskbar `hwnd` and reads its flags exactly once (via
  `find_taskbar_window`/`read_appbar_flags`), reusing both for `state_from_flags` (derive current
  state) and `write_appbar_state` (write the forced state) rather than going through a
  separate query-then-set pair -- a review fix that removed a redundant second
  `find_taskbar_window` lookup per session; the earlier `query_taskbar_autohide_state` function it
  replaced was deleted as dead code once nothing called it anymore.
- `settings_ui::run_settings_window` was restructured into a loop (`SettingsExit` enum: `Saved`,
  `Cancelled`, `EditCustomZone(SettingsState)`) because eframe/winit can't nest one native event
  loop inside another — clicking "Edit Custom Zone" closes the Settings window, runs
  `zone_overlay::run_zone_editor_overlay` to completion, folds the result into `SettingsState`, and
  re-opens Settings sequentially (same pattern as `app::run_event_loop`'s tray -> Settings sequencing).
- Issue #26's manual Custom X/Y/width/height numeric-entry fields/methods
  (`custom_x`/`custom_y`/`custom_width`/`custom_height`/`custom_region_error`,
  `parse_custom_preset`/`parse_field`) were removed from `SettingsState` entirely, superseded by the
  overlay. `SettingsState::set_custom_preset` now takes a `CustomRegion` directly (the overlay's
  output) instead of parsing text fields.
- Only the Custom preset is wired to the overlay (`PinnedEdges::NONE`, all 8 handles) per #28's
  acceptance criteria; Left/Right base-preset editing via the overlay (`PinnedEdges::LEFT_HALF`/
  `RIGHT_HALF` already exist as consts) is a reasonable future extension but out of scope.
- The overlay's rendering/DX12/DirectComposition setup and the taskbar OS calls are untested by
  convention (no headless GPU/taskbar in CI) — same boundary as `settings_ui::run_settings_window`/
  `tray_ui::Tray::new`/`win32`'s other OS-facing functions.
