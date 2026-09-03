# Per-game taskbar auto-hide is coupled to preset-apply, not a separate action

**Status:** accepted

`TaskbarAutohideState` was previously documented as fully independent of the borderless-toggle code path — its own hotkey, its own two-state flip, never read or written when a preset gets applied. Per-game management (ticket #21) breaks that independence deliberately: both the borderless-toggle hotkey and the tray's "Apply to Window" now also set the target tracked game's taskbar-auto-hide value (via the existing absolute-set primitive, not the toggle) whenever they apply that game's active preset. The alternative — only the tray path setting it — was rejected because it would make the same tracked game's taskbar behavior depend on which of the two apply paths triggered it, which is more surprising than the coupling itself.

Issue #42 later added `TaskbarHideMode` (FullyHidden/HoverReveal), a global setting for *which technique* any hide call site uses — see `CONTEXT.md`'s "Taskbar hide mode" entry. That's orthogonal to this ADR's coupling decision (which only governs *whether* the taskbar gets hidden as a side effect of applying a preset), but it does mean the global hotkey's hide *direction* is no longer untouched in the literal sense the Consequences bullet below originally claimed — see the correction there.

Issue #43 later added automatic taskbar restore on a hidden-window's close (and on GameFrame quitting via the tray "Exit" item), also orthogonal to this ADR's coupling decision. Its reference-counted armed-window state is a new, fully separate piece of static state (`win32::ARMED_TASKBAR_RESTORE`) — never read or written by the manual global hotkey's own toggle — so it preserves the same independence this ADR requires.

## Consequences

- The existing global taskbar auto-hide hotkey's on/off toggle *concept* is untouched and remains available as a manual, tracked-game-independent toggle — the two mechanisms can disagree (e.g. a manual toggle right after a game-triggered auto-hide), and that's accepted, not resolved here. As of issue #42, the hotkey's "on" direction is mode-aware (it hides via whichever technique `taskbar_hide_mode` currently selects), but this doesn't change the toggle's independence from tracked-game state — only which OS mechanism it uses to hide.
- A window whose process is not a tracked game causes neither a preset-apply nor a taskbar change from either path — see `CONTEXT.md`'s "Applying a preset to a window whose process is not a tracked game does nothing" relationship.
