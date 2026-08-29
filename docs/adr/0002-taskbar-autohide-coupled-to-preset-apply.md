# Per-game taskbar auto-hide is coupled to preset-apply, not a separate action

**Status:** accepted

`TaskbarAutohideState` was previously documented as fully independent of the borderless-toggle code path — its own hotkey, its own two-state flip, never read or written when a preset gets applied. Per-game management (ticket #21) breaks that independence deliberately: both the borderless-toggle hotkey and the tray's "Apply to Window" now also set the target tracked game's taskbar-auto-hide value (via the existing absolute-set primitive, not the toggle) whenever they apply that game's active preset. The alternative — only the tray path setting it — was rejected because it would make the same tracked game's taskbar behavior depend on which of the two apply paths triggered it, which is more surprising than the coupling itself.

## Consequences

- The existing global taskbar auto-hide hotkey is untouched and remains available as a manual, tracked-game-independent toggle — the two mechanisms can disagree (e.g. a manual toggle right after a game-triggered auto-hide), and that's accepted, not resolved here.
- A window whose process is not a tracked game causes neither a preset-apply nor a taskbar change from either path — see `CONTEXT.md`'s "Applying a preset to a window whose process is not a tracked game does nothing" relationship.
