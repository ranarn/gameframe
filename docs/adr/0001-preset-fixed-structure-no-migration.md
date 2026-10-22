# Preset set is a fixed structure, not a free-form list — and config.toml is not migrated

**Status:** accepted — the no-migration stance was superseded for the Zone change by `0003-zone-config-migration.md`; the preset structure it describes no longer exists.

The preset set (originally `Left`, `Right`, `HalfLeft`, `HalfRight`, plus an optional `Custom`) was previously a free-form `Vec<Preset>`: any number of any variant could be added or removed, and editing meant removing an entry and re-adding it. That no longer matches the domain — the base presets are mandatory and non-deletable, and at most one `Custom` can exist — so the config's `presets: Vec<Preset>` becomes a fixed structure with one field per base preset plus `custom: Option<Custom>`. This makes "always the fixed set of base presets, at most one Custom" a type-level invariant instead of a UI-only or runtime-checked rule.

This is a breaking change to `config.toml`'s shape (also removing `FullScreen` and renaming `LeftHalf`/`RightHalf` to `Left`/`Right` with an added adjustable depth-from-edge value). No migration path is provided: a config that doesn't match the new schema is discarded and replaced with defaults on next load. GameFrame is pre-release with a single user, so the cost of a migration outweighs its value here.

Issue #37 later removed the `HalfLeft`/`HalfRight` base presets entirely, leaving `Left`, `Right`, and `Custom` as `PresetKind`'s three variants — this ADR's "no migration" stance covers that removal too, without a new ADR: a stored config referencing a since-removed `PresetKind` (including `HalfLeft`/`HalfRight` themselves) simply fails to deserialize and resets to defaults, the same as any other schema mismatch under this decision.

## Consequences

- `win32.rs`'s `active_preset()` "first in list" MVP hack (issue #6) is untouched by this decision — resolving it belongs to per-game preset assignment (ticket #21), not to this data-model change.
- Any local `config.toml` predating this change will silently reset to defaults the next time GameFrame runs after this lands.
- Any local `config.toml` with a tracked game still assigned `HalfLeft`/`HalfRight` (or any other future removed `PresetKind`) will silently reset to defaults the next time GameFrame runs after that variant is removed (see issue #37).
