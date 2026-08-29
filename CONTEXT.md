# GameFrame

GameFrame positions game windows into predefined screen regions via hotkeys or per-game auto-apply. This context defines the vocabulary for how a region is described, saved, and selected.

## Language

**Preset**:
A named window-placement region that can be applied to a target window. Exactly five kinds exist: four **base presets** and at most one **Custom** preset.
_Avoid_: Profile, Layout

**Base preset**:
One of the four presets (**Left**, **Right**, **HalfLeft**, **HalfRight**) that always exist in the preset set and cannot be deleted. Their adjustable values (where present) are edited in place, never by removing and re-adding.

**Left / Right**:
A base preset anchored to the screen's left/right edge, full height, with an adjustable **depth-from-edge**.

**HalfLeft / HalfRight**:
A base preset anchored to the screen's left/right edge, full height, permanently fixed at 50% width — not adjustable.
_Avoid_: confusing these with Left/Right at a 50% depth-from-edge setting; HalfLeft/HalfRight are a structurally distinct, non-adjustable pair.

**Depth-from-edge**:
The percentage (10%-90%) of screen width a **Left** or **Right** preset extends from its anchored screen edge. Stored as a percentage and resolved against the target monitor's geometry at apply-time, not baked into absolute pixels.

**Custom preset**:
The single, optional freeform preset built by drawing a region with the zone-editing overlay (see **Zone**), stored as absolute screen coordinates. At most one exists at a time; drawing a new one replaces it.

**Zone**:
The overlay's live, in-progress region while the user is drawing or resizing it. A Zone becomes the Custom preset only once saved.
_Avoid_: using "Zone" and "Custom preset" interchangeably — a Zone is the editing-time state, the Custom preset is what gets persisted.

**Active preset**:
The preset currently assigned to a given game. Displaying it is this context's concern; the assignment mechanism (which preset a game is bound to) belongs to per-game management, not the preset data model itself.

## Relationships

- The preset set holds exactly one of each **base preset** (**Left**, **Right**, **HalfLeft**, **HalfRight**) plus at most one **Custom preset** — not a free-form list.
- A **Zone**, once saved via the overlay, becomes the **Custom preset**.
- Each game is assigned exactly one **active preset** (base or Custom).

## Flagged ambiguities

- "Zone" was initially used loosely for both the overlay's editing surface and the saved result — resolved: Zone is editing-time only, Custom preset is the persisted outcome.
