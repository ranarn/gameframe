# The game cursor is drawn from outside the game, never injected

**Status:** accepted

Some games (Last Epoch, Path of Exile before it got its own setting) have a small cursor that gets lost among effects. YoloMouse, the tool used for this before, swaps the cursor the game itself sets: it injects into the game process and replaces the game's cursor handles. That reaches every game, but it means code running inside the game, which is the thing anti-cheat looks for; YoloMouse needs per-vendor whitelisting for that reason.

GameFrame's **game cursor** (see `CONTEXT.md`) never touches the game process. A dedicated thread owns a click-through, topmost, per-pixel-alpha layered window and moves it onto the cursor about every 4 ms. It works in any game GameFrame can already handle, because GameFrame games run borderless/windowed, where a topmost window is composited above them. Fullscreen-exclusive games would cover it, but GameFrame's model doesn't use exclusive fullscreen. Cost: the drawing trails the hardware cursor by about one composited frame.

The game's own cursor is a problem a window can't solve: Last Epoch (Unity) sets a custom hardware cursor (`GetCursorInfo` over the game returns its own `HCURSOR`, not the arrow), and Windows draws the hardware cursor above every window, our topmost one included, so it sat on top of the game cursor. While the game cursor is shown, GameFrame therefore hides the real cursor with `MagShowSystemCursor(FALSE)`, the Magnification API's global switch that screen magnifiers use to draw their own pointer, and shows it again the moment the game cursor hides. This still never touches the game process. It was checked in Last Epoch before it was built: the game's cursor disappears, and Windows shows the cursor again by itself when the hiding process exits, so a crash can't leave it hidden. It doesn't change `GetCursorInfo`'s `CURSOR_SHOWING`, so "hide when the game hides its cursor" still sees the game's own hiding. A setting (`hide_real_cursor`, default on) turns this off for anyone who wants both, e.g. a ring around the game's own cursor.

Like YoloMouse's, it exists only in and over the game: it is shown while the top-level window *under the cursor* (`WindowFromPoint`, which skips our own click-through window) belongs to a tracked game with "Game cursor" ticked. Keying it to the foreground window instead drew it over other windows and the other monitor whenever the game had focus.

One rasterizer (`cursor_style`: signed-distance shapes, border, glow, one-pixel anti-aliasing) draws the overlay and the Settings previews, so they are pixel-identical.

## Considered and removed: replacing the system cursor

The first version also offered swapping the Windows arrow and hand cursors (`SetSystemCursor`) while the game was in front, restoring the scheme with `SPI_SETCURSORS` afterwards. That would have meant no lag and no double cursor. It did nothing in Last Epoch: the game sets its own cursor, so the system arrow never appears there (hiding the real cursor outright, above, is what handles that game). A swapped system cursor is also global state that outlives a crash (it needed a marker file and a panic hook to restore), and restoring it resets cursors other tools set. Without the game that motivated it, it wasn't worth that, so it was removed. Configs that still carry its `method` key (and the also-removed `scope` key) load unchanged; unknown keys are ignored.

## Consequences

- No anti-cheat exposure: nothing is loaded into, hooked in or read from the game process. The only per-game input is the executable path of the window under the cursor, the same signal the apply paths already use.
- The overlay window is a tool window, so the "Apply to Window" picker's existing filter skips it. It moves via `SetWindowPos`, so it raises `EVENT_OBJECT_LOCATIONCHANGE` that the drift watcher ignores after one `is_applied_window` lookup.
- A topmost layered window over a game makes DWM composite that game's frames instead of letting them flip straight to the screen, which can add a little latency. The overlay window is kept tight to the pointer (the large pulse canvas exists only while a pulse plays) to keep composition cheap.
- The overlay thread is per-monitor-DPI-aware on its own (`SetThreadDpiAwarenessContext`), so the rest of the process's DPI behavior is unchanged.
