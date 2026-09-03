# GameFrame

GameFrame positions game windows into predefined screen regions via hotkeys or per-game auto-apply. This context defines the vocabulary for how a region is described, saved, and selected.

## Language

**Preset**:
A named window-placement region that can be applied to a target window. Exactly three kinds exist: two **base presets** and at most one **Custom** preset.
_Avoid_: Profile, Layout

**Base preset**:
One of the two presets (**Left**, **Right**) that always exist in the preset set and cannot be deleted. Their adjustable **depth-from-edge** is edited in place, never by removing and re-adding.

**Left / Right**:
A base preset anchored to the screen's left/right edge, full height, with an adjustable **depth-from-edge**.

**Depth-from-edge**:
The percentage (10%-90%) of screen width a **Left** or **Right** preset extends from its anchored screen edge. Stored as a percentage and resolved against the target monitor's geometry at apply-time, not baked into absolute pixels.

**Custom preset**:
The single, optional freeform preset built by drawing a region with the zone-editing overlay (see **Zone**), stored as absolute screen coordinates. At most one exists at a time; drawing a new one replaces it.

**Zone**:
The overlay's live, in-progress region while the user is drawing or resizing it. A Zone becomes the Custom preset only once saved.
_Avoid_: using "Zone" and "Custom preset" interchangeably — a Zone is the editing-time state, the Custom preset is what gets persisted.

**Active preset**:
The preset currently assigned to a given **tracked game** — stored as a reference to one of the three preset kinds, never a copy, so editing a base preset or replacing the Custom preset immediately changes what every game assigned to it applies.

**Default preset (for new tracked games)**:
The single preset kind that will be assigned to the next game added as a **tracked game**, set by clicking its thumbnail in Settings' preset row (highlighted there to show the current choice) and persisted to config. Distinct from **Active preset**: changing the default never changes any already-tracked game's existing assignment — it only determines what a *future* newly-tracked game starts with.
_Avoid_: confusing this with **Active preset** — the default is a single global setting that seeds a new tracked game's initial Active preset; after that, the two are unrelated.

**Tracked game**:
A process, identified by its **executable path**, that has been added to GameFrame's per-game list. Added by picking it from the "Apply to Window" picker — the same action also applies its active preset immediately — and persists across sessions; picking an already-tracked game again does not create a duplicate entry.
_Avoid_: "game" alone for a window that merely looks like a game but hasn't been added to this list yet.

**Executable path**:
A tracked game's identity — the full path to its running process's executable. Two windows belonging to processes with the same executable path are the same tracked game.

**Auto-apply**:
A per-tracked-game setting: whether this game's active preset should be applied the moment the game launches, versus only on demand via the hotkey or tray menu. Defaults to on for a newly-added tracked game.

**Per-game taskbar auto-hide**:
A per-tracked-game setting: whether applying this game's active preset should also hide the Windows taskbar. Defaults to on for a newly-added tracked game. Distinct from the manual, global taskbar auto-hide toggle, which is unrelated to any tracked game.

**Taskbar hide mode**:
A global setting (`TaskbarHideMode`, config field `taskbar_hide_mode`) choosing *which technique* is used whenever the taskbar is hidden: **FullyHidden** (the default — the taskbar stays hidden with no hover-reveal, via `ShowWindow`) or **HoverReveal** (Windows' native auto-hide, which reappears on mouse hover near the screen edge, via the appbar auto-hide mechanism). This is a third axis, orthogonal to both **Per-game taskbar auto-hide** and the manual global taskbar auto-hide toggle — both of those decide *whether* the taskbar gets hidden; this setting decides *how* it's hidden once that decision is made.

## Relationships

- The preset set holds exactly one of each **base preset** (**Left**, **Right**) plus at most one **Custom preset** — not a free-form list.
- A **Zone**, once saved via the overlay, becomes the **Custom preset**.
- Each **tracked game** is assigned exactly one **active preset** (base or Custom), plus its own **auto-apply** and **per-game taskbar auto-hide** settings.
- Applying a preset to a window whose process is not a **tracked game** does nothing — there is no untracked fallback.
- A newly-added **tracked game**'s initial **active preset** is seeded from the current **default preset (for new tracked games)** — a one-time copy at add-time, not a live reference, so a later change to the default never retroactively changes that game's assignment.

## Flagged ambiguities

- "Zone" was initially used loosely for both the overlay's editing surface and the saved result — resolved: Zone is editing-time only, Custom preset is the persisted outcome.
