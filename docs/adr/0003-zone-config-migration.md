# A single Zone replaces presets, and legacy configs are migrated

**Status:** accepted — supersedes ADR 0001's "no migration" stance for this change (ADR 0001's rule still applies to corrupt or unrecognised configs).

GameFrame's config used to hold a fixed preset set (`Left`/`Right` with `depth_percent`, an optional `Custom` region), a `default_for_new_games`, and a `preset` reference on every tracked game. The product now has one concept instead: a single optional **Zone** (`zone: Option<Zone>`, absolute x/y/width/height) shared by all tracked games (see `CONTEXT.md`).

ADR 0001 chose to discard any config that doesn't match the current schema, because the project was pre-release with one user. That cost calculation changed: the user now has real tracked games, hotkeys and a drawn Custom region, and losing them for a vocabulary change is a worse trade than a small conversion function.

## Decision

`profile_store::load` tries, in order:

1. **New shape** — a config without a `[presets]` table is parsed as the Zone-based `Config` and returned as-is.
2. **Legacy conversion** — a config with a `[presets]` table is converted by the pure function `convert_legacy_config` (parsed TOML in, `Option<Config>` out; no filesystem access): `presets.custom` becomes `zone` with identical coordinates (absent means `None`); tracked games (exe path, display name, auto-apply, taskbar flag), hotkeys and `taskbar_hide_mode` are kept; depths, `default_for_new_games` and each game's `preset` are dropped. The new shape is written back immediately, so the conversion runs once.
3. **Defaults** — corrupt TOML, or a legacy file that cannot be converted (e.g. the pre-#26 array-of-presets shape), is replaced with a default config, which has no Zone.

The legacy marker is checked before the new-shape parse because a legacy file would otherwise also deserialize as the new shape (serde ignores the unknown `presets` key) and silently lose its Custom region.

## Consequences

- Users who had only Left/Right presets (no Custom) end up with no Zone and must draw one; every apply path then does nothing and logs "no Zone is set". Left/Right have no equivalent and are not converted into a Zone.
- A game that was assigned Left or Right now gets the Zone like every other game.
- The legacy structs live in `profile_store.rs` and can be deleted once nobody is expected to hold a preset-era config.
- Downgrade hazard: once migrated, the file has no `[presets]` table, so an older (preset-era) GameFrame build reading it treats it as an unrecognised schema and resets to defaults, losing the games and the Zone. Migration is one-way.
- A file that cannot be converted, or is corrupt TOML, is copied to `config.toml.bak` (and logged) before being replaced with defaults, so nothing is lost without trace. If the migrated config cannot be written back, the error is logged and the converted config is still used; the conversion repeats on the next load.
