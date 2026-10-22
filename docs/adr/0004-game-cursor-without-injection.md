# The game cursor is drawn from outside the game, never injected

**Status:** accepted

Some games (Last Epoch, Path of Exile before it got its own setting) have a small cursor that gets lost among effects. YoloMouse, the tool used for this before, swaps the cursor the game itself sets: it injects into the game process and replaces the game's cursor handles. That reaches every game, but it means code running inside the game, which is the thing anti-cheat looks for; YoloMouse needs per-vendor whitelisting for that reason.

GameFrame's **game cursor** (see `CONTEXT.md`) never touches the game process. It has two methods, chosen in Settings:

1. **Overlay** (default). A dedicated thread owns a click-through, topmost, per-pixel-alpha layered window and moves it onto the cursor about every 4 ms. It works in any game GameFrame can already handle, because GameFrame games run borderless/windowed, where a topmost window is composited above them. Fullscreen-exclusive games would cover it, but GameFrame's model doesn't use exclusive fullscreen. Costs: the game's own cursor stays visible underneath (fine for rings and reticles; the arrow sits on top of it), and the drawing trails the hardware cursor by about one composited frame.
2. **Replace system cursor.** While the game cursor is shown, `SetSystemCursor` swaps the Windows arrow and hand cursors for the rendered pointer, and `SPI_SETCURSORS` reloads the user's scheme when it isn't. This has no lag and no double cursor, but only games that use the standard Windows cursor show it. Last Epoch's own support article points users at the Windows pointer-size setting, which suggests it does.

Both methods render with one software rasterizer (`cursor_style`: signed-distance shapes, border, glow, one-pixel anti-aliasing), so the Settings preview, the overlay and the swapped cursor are pixel-identical.

## Consequences

- No anti-cheat exposure: nothing is loaded into, hooked in or read from the game process. The only per-game input is the foreground window's executable path, the same signal the apply paths already use.
- A swapped system cursor is global state that would outlive a crash. While a swap is active, `cursor-swap.active` exists in the config directory, and the next start restores the scheme when it finds the file. A clean exit (tray Exit, or GameFrame relaunching itself elevated) restores it directly.
- Restoring uses `SPI_SETCURSORS`, which reloads the scheme from the registry. A cursor some other tool set with `SetSystemCursor` (not persisted in the registry) is reset too.
- The overlay window is a tool window, so the "Apply to Window" picker's existing filter skips it. It moves via `SetWindowPos`, so it raises `EVENT_OBJECT_LOCATIONCHANGE` that the drift watcher ignores after one `is_applied_window` lookup.
- A topmost layered window over a game makes DWM composite that game's frames instead of letting them flip straight to the screen, which can add a little latency. Replace system cursor avoids this, since the overlay is then only shown during a pulse. The overlay window is kept tight to the pointer (the large pulse canvas exists only while a pulse plays) to keep composition cheap.
- A panic hook restores swapped cursors, because the release build aborts on panic and no `Drop` runs.
- Known edge: when GameFrame relaunches itself elevated while a swap is active, the old instance restores the cursors as it exits. If the new instance has already swapped by then, the cursor stays normal until it next becomes visible.
- The overlay thread is per-monitor-DPI-aware on its own (`SetThreadDpiAwarenessContext`), so the rest of the process's DPI behavior is unchanged.
