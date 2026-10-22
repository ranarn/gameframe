Game cursor (YoloMouse replacement), branch feature/cursor-highlight, true as of the commit that adds src/cursor_style.rs.

- `src/cursor_style.rs` — pure: `CursorSettings` (config `[cursor]`, `#[serde(default)]` struct-wide, re-exported by `profile_store`), SDF rasterizer (`render_pointer`, `draw_pulse_ring`, canvases `overlay_canvas`/`cursor_canvas`), `pulse_frame`, `ShakeDetector`, `should_show`. Unit tested.
- `src/cursor_overlay.rs` — OS-facing: `CursorOverlay::start/reload/pulse`, dedicated thread (per-monitor-DPI-aware via SetThreadDpiAwarenessContext) owning a WS_EX_LAYERED|TRANSPARENT|TOPMOST|TOOLWINDOW|NOACTIVATE window class `GameFrameGameCursor`, moved every ~4 ms using a high-resolution waitable timer. `ReplaceSystemCursor` method = SetSystemCursor(OCR_NORMAL, OCR_HAND) + SPI_SETCURSORS restore; crash marker `cursor-swap.active` in the config dir.
- `app.rs` reloads the overlay after Settings closes and after "Apply to Window"; hotkey `toggle_game_cursor` flips+persists `cursor.enabled` and pulses.
- Per-game flag `TrackedGame.game_cursor` (serde default true).

Gotchas:
- Own thread is required: the tray thread blocks while Settings/zone editor (eframe) is open.
- Don't use DwmFlush for pacing: it can block when nothing composes; the waitable timer is used instead.
- Testing from PowerShell: `FindWindow(class, $null)` passes "" not NULL in PS — use `[NullString]::Value`. Screen capture needs CAPTUREBLT (BitBlt rop 0x40CC0020) to include layered windows; .NET's CopyPixelOperation enum rejects it.
- The repo is NOT rustfmt-formatted: never run `cargo fmt` on the whole crate (it rewrites ~20 files).
See ADR 0004 and `mem:core`.
