Game cursor (YoloMouse replacement), branch feature/cursor-highlight. True as of the commit "fix: game cursor only over the game; Settings tabs; log file" (follow-up to 68d9933).

- `src/cursor_style.rs` — pure: `CursorSettings` (config `[cursor]`, `#[serde(default)]` struct-wide, re-exported by `profile_store`), SDF rasterizer (`render_pointer`, `draw_pulse_ring`, canvases `overlay_canvas`/`cursor_canvas`), `pulse_frame`, `ShakeDetector`, `should_show(settings, over_game, cursor_showing)`. Unit tested.
- `src/cursor_overlay.rs` — OS-facing: `CursorOverlay::start/reload/pulse`, dedicated thread (per-monitor-DPI-aware via SetThreadDpiAwarenessContext) owning a WS_EX_LAYERED|TRANSPARENT|TOPMOST|TOOLWINDOW|NOACTIVATE window class `GameFrameGameCursor`, moved every ~4 ms using a high-resolution waitable timer.
- Visibility: shown only while `WindowFromPoint(cursor)` -> `GetAncestor(GA_ROOT)` is a cursor-enabled tracked game's window (exe lookup cached per root hwnd). Verified live: WindowFromPoint skips our own click-through overlay (no flicker), and the game having focus no longer matters.
- Last Epoch uses custom HARDWARE cursors (own HCURSORs via GetCursorInfo, switching on hover ~2/s), which Windows draws above every window — z-order can't put ours on top. `CursorShape::GameOwn` builds on it instead: `read_cursor_image` (GetIconInfo + GetDIBits; works cross-process, verified in LE: 32x32 32bpp with alpha) -> `cursor_style::cursor_image` -> `render_game_own` (enlarged copy behind + border/glow outside the alpha outline via `distance_transform`, Felzenszwalb EDT). Cached per handle in the overlay thread. Enlargement pivot is `TIP_PIVOT` (bottom-right corner of the hotspot pixel) — tuned in Last Epoch by eye: pivot at the corner (0,0) put the game's cursor ON the copy's left/top edge, center (0.5) was one step short. Settings previews use `render_preview` with a stand-in arrow.
- DON'T retry hiding the game's cursor with `MagShowSystemCursor(FALSE)`: measured in LE (A/B script, hide once / every 4 ms / on handle change), the cursor reappears at stale positions whenever the game sets one -> laggy, stuck cursor. Removed; see ADR 0004.
- `app.rs` reloads the overlay after Settings closes and after "Apply to Window"; hotkey `toggle_game_cursor` flips+persists `cursor.enabled` and pulses.
- Per-game flag `TrackedGame.game_cursor` (serde default true).

Removed (see ADR 0004): `CursorMethod::ReplaceSystemCursor` (SetSystemCursor) did nothing in Last Epoch, which sets its own cursor; `CursorScope::Everywhere` was unwanted. Old configs with `method`/`scope` keys still load (serde ignores unknown keys; tested).

Gotchas:
- Own thread is required: the tray thread blocks while Settings/zone editor (eframe) is open.
- Don't use DwmFlush for pacing: it can block when nothing composes; the waitable timer is used instead.
- Testing from PowerShell: `FindWindow(class, $null)` passes "" not NULL in PS — use `[NullString]::Value`. Screen capture needs CAPTUREBLT (BitBlt rop 0x40CC0020) to include layered windows; .NET's CopyPixelOperation enum rejects it.
- The repo is NOT rustfmt-formatted: never run `cargo fmt` on the whole crate (it rewrites ~20 files).
See ADR 0004, `mem:core` and `mem:conventions` (logging).
