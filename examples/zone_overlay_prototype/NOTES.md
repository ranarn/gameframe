# Zone overlay drag/resize — prototype notes

Wayfinder ticket [#18](https://github.com/ranarn/gameframe/issues/18), map #14.

## Question

Prototype the zone-editing overlay interaction end to end: drag corner-to-corner
to define a region, then adjust it via edge/corner resize handles. Must support
zones extending below the taskbar (evaluate auto-hiding the taskbar while
editing). Decide whether resize handles are locked on certain edges depending
on preset type (e.g. "Left" only draggable on its right edge, screen edge
locked). Also nail down how click-through + translucent rendering is achieved
on Windows, and whether the overlay carries its own Save/Cancel controls.

## Run it

```
cargo run --example zone_overlay_prototype
```

Commands: `new` (freeform), `edit left|right|custom` (pre-load an existing
preset's bounds), `start x y`, `drag x y`, `commit`, `grab <handle>`, `save`,
`cancel`, `q`.

## What the pure module (`zone_editor.rs`) answers

- **State machine**: `Empty → Dragging → Placed → Resizing → Placed → …`,
  with `Cancel` legal from any state and `Save` legal only from `Placed`
  (verified: attempting `save` mid-resize is correctly rejected — you must
  `commit` first).
- **Pinned edges, not preset-specific logic**: a `PinnedEdges{left,top,right,bottom}`
  struct (not tied to ticket #20's still-undecided `Preset` enum) drives which
  of the 8 handles are enabled. Verified live: editing `left` exposes only `e`;
  editing `right` exposes only `w`; a freeform/custom zone exposes all 8. A
  corner handle needs *both* adjacent edges free — a Left/Right preset (3
  pinned edges) never gets a corner handle, only its one free edge.
- **Taskbar auto-hide**: forced on for the full duration a zone is being
  edited (drawing, placed, or mid-resize), restored on `save` or `cancel`.
  Verified a freeform zone can be drawn down to `y=900..1080` on a 1080px-tall
  monitor (i.e. into the taskbar's strip) with no special-casing needed —
  the monitor geometry used is the full screen, not a taskbar-excluded work
  area, so nothing prevents the drag/resize edges from reaching it.

## Visual prototype (`../zone_overlay_visual/`)

The text TUI above answers the *interaction model* but can't show the
visual feel, so a second throwaway prototype — a real on-screen `eframe`
window, reusing this same `zone_editor.rs` pure module via `#[path = ...]`
— was built and driven live. Run with `cargo run --example
zone_overlay_visual`.

**Transparency was NOT "just works" as first assumed — corrected after live
testing.** The initial assumption (below, from context7 docs alone) that
`ViewportBuilder::with_transparent(true)` is sufficient turned out to be
wrong on real hardware: the window rendered fully opaque black. Root cause
(confirmed via a temporary diagnostic `log::Log` impl, since this project
installs no logger by default and the warning was being silently
swallowed): `egui_wgpu::winit: Transparent window was requested, but the
active wgpu surface does not support a CompositeAlphaMode with
transparency.` The default wgpu adapter/backend on this machine (an AMD
GPU, Vulkan) reports only `CompositeAlphaMode::Opaque` for a plain
top-level-window-bound swapchain — and this is not Vulkan-specific; DX12's
*default* HWND-bound swapchain kind is `[Opaque]`-only too.

This is the exact same bug the sibling `poe-gizmos` project already hit and
fixed (same `eframe` 0.36.1) — see its `src/overlay.rs` module doc comment,
"Root cause of the opacity bug", for the full writeup including a rejected
`WS_EX_LAYERED`/colorkey alternative that also doesn't work against a
modern flip-model swapchain. **The fix, applied here and confirmed live:**
force the DX12 backend with a DirectComposition-backed swapchain
(`Dx12SwapchainKind::DxgiFromVisual`, binding the swapchain to an
`IDCompositionVisual` instead of the raw `HWND`) via
`NativeOptions::wgpu_options`. Forcing DX12 alone (default HWND-bound
swapchain) is confirmed *insufficient* — both pieces are required together.
Trade-off: this removes wgpu's automatic backend fallback, so a machine
without a working DX12 adapter would fail to open the overlay at all
(non-fatal, logged) rather than falling back to Vulkan/GL.

**Verdict from driving it live (Jonas, 2026-08-29):** transparency, drag-to-draw,
pinned-edge handle locking (Left/Right presets), and Save/Cancel all work as
designed. Some visual polish remains (not investigated further here — out of
scope for this ticket, belongs to the real implementation slice, not a
throwaway prototype): exact colors/sizing of the dim overlay and handles,
cursor icons on hover, etc.

## Platform questions resolved via context7 + live testing

- **Click-through / translucency**: no raw `WS_EX_LAYERED`/`WS_EX_TRANSPARENT`
  Win32 calls needed for the *click-through* part — `egui`'s `ViewportBuilder`
  (eframe's existing winit backend — already a dependency, see
  `settings_ui.rs`) exposes `.with_transparent(true)`, `.with_decorations(false)`,
  `.with_taskbar(false)`, and `.with_mouse_passthrough(bool)` directly; the
  latter is also togglable at runtime via
  `ctx.send_viewport_cmd(ViewportCommand::MousePassthrough(bool))`, the same
  pattern `settings_ui.rs` already uses for `ViewportCommand::Close`. The
  *translucency* part needed more — see "Visual prototype" above: on this
  hardware it additionally requires forcing wgpu's DX12 backend with a
  DirectComposition-backed swapchain, not just the `ViewportBuilder` flag.
- **Recommended verdict on passthrough**: don't toggle it at all for the
  editing session — keep the overlay fully input-capturing (`mouse_passthrough`
  omitted/false) for its whole lifetime, same modal blocking pattern as
  `run_settings_window`. True runtime passthrough toggling (as the
  `egui_overlay` crate's docs describe: enable passthrough except when
  `ctx.wants_pointer_input()`) has a chicken-and-egg problem for us — before
  any zone/handle exists there's nothing to hover, so the very first
  corner-click that starts the drag would fall through. A short modal
  edit session doesn't need passthrough at all; it only matters for a
  persistent always-on HUD, which this isn't.
- **Save/Cancel controls**: the overlay carries its own controls (not some
  other confirm mechanism) — same `Save`/`Cancel` pair `settings_ui.rs`
  already renders, reusable as a small non-passthrough egui panel fixed to
  the overlay (e.g. top-center), consistent with existing UI conventions.
- **Taskbar auto-hide mechanism**: reuse `win32::toggle_taskbar_autohide` /
  `TaskbarAutohideState` / `set_appbar_state` as-is rather than adding new
  Win32 calls — that machinery already exists (built for the global-hotkey
  taskbar toggle). One gap: `set_appbar_state` is private and
  `toggle_taskbar_autohide` only *flips* the state. The zone editor needs a
  force-on-then-restore-previous pair instead of a toggle (so it doesn't
  accidentally flip an already-hidden taskbar to visible on open); this
  needs a small `pub` addition to `win32.rs`, not new OS-facing code.

## Verdict

Design confirmed. See ticket #18's `## Resolution` for the summary copied
from this file.
