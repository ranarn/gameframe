# GameFrame

GameFrame positions game windows into a single user-drawn screen area via hotkeys or per-game auto-apply. This context defines the vocabulary for how that area is described, saved, and applied.

## Language

**Zone**:
The single, optional, user-drawn area — stored as absolute screen coordinates (x, y, width, height) — that every **tracked game**'s window is placed into. Drawn and edited with the zone-editing overlay; at most one exists at a time and drawing a new one replaces it. With no Zone set, every apply path does nothing (and logs that no Zone is set, distinctly from "not a tracked game"). Resolved against the target monitor's geometry at apply-time: a Zone that starts off the monitor is rejected, one that overflows it is clamped.
_Avoid_: Preset, Profile, Layout, Region (as a user-facing or persisted concept — "region" only names the resolved pixel rectangle inside `region_calc`).

**Tracked game**:
A process, identified by its **executable path**, that has been added to GameFrame's per-game list. Added by picking it from the "Apply to Window" picker — the same action also applies the Zone immediately — and persists across sessions; picking an already-tracked game again does not create a duplicate entry.
_Avoid_: "game" alone for a window that merely looks like a game but hasn't been added to this list yet.

**Executable path**:
A tracked game's identity — the full path to its running process's executable. Two windows belonging to processes with the same executable path are the same tracked game.

**Auto-apply**:
A per-tracked-game setting: whether the Zone should be applied to this game the moment it launches, versus only on demand via the hotkey or tray menu. Defaults to on for a newly-added tracked game.

**Hide taskbar on apply**:
A single global setting (config field `hide_taskbar_on_apply`, default on): whether applying the Zone to any tracked game also hides the Windows taskbar. Shown in Settings as "Hide taskbar when the Zone is applied", next to the **Taskbar hide mode** choice. Distinct from the manual, global taskbar auto-hide toggle, which is unrelated to any tracked game. As of issue #43, a Zone-apply hide is auto-restored with no hotkey press required, either when the game window that triggered it closes or when GameFrame itself quits via the tray "Exit" item while a hide is still in effect; the underlying state is reference-counted across every tracked game currently relying on the hide (not just the single most recent one), so the taskbar only actually reappears once every such window has closed. It was a per-game setting before issue #49 (see ADR 0003); a config that still has per-game flags converts on load to on if any game had it on.

**Taskbar hide mode**:
A global setting (`TaskbarHideMode`, config field `taskbar_hide_mode`) choosing *which technique* is used whenever the taskbar is hidden: **FullyHidden** (the default — the taskbar stays hidden with no hover-reveal, via `ShowWindow`) or **HoverReveal** (Windows' native auto-hide, which reappears on mouse hover near the screen edge, via the appbar auto-hide mechanism). This is a third axis, orthogonal to both **Hide taskbar on apply** and the manual global taskbar auto-hide toggle — both of those decide *whether* the taskbar gets hidden; this setting decides *how* it's hidden once that decision is made.

## Relationships

- There is at most one **Zone**, and it is shared by every **tracked game** — a game does not carry its own placement.
- Saving a new **Zone** in Settings immediately re-applies it to every running tracked game. The Zone is also persisted at that moment (and on "Clear"), independent of Settings' own Save/Cancel, so the on-disk Zone and the live windows never disagree; Cancel discards only the other unsaved fields.
- Each **tracked game** has its own **auto-apply** setting; **hide taskbar on apply** is global, not per game.
- Applying the Zone to a window whose process is not a **tracked game** does nothing — there is no untracked fallback.
- With no **Zone** set, applying does nothing for any game; the hotkey's restore direction still works.

## Flagged ambiguities

- Before the Zone model, GameFrame had "Presets" (Left/Right with depth-from-edge, plus one Custom), a per-game "Active preset" and a "Default preset for new tracked games"; "Zone" then meant only the overlay's in-progress editing state. All of that is retired: **Zone** is now the one persisted concept (see `docs/adr/0003-zone-config-migration.md`).