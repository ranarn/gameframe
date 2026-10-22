# A single Zone replaces presets, and legacy configs are migrated

**Status:** accepted — supersedes ADR 0001's "no migration" stance for this change (ADR 0001's rule still applies to corrupt or unrecognised configs).

GameFrame's config used to hold a fixed preset set (`Left`/`Right` with `depth_percent`, an optional `Custom` region), a `default_for_new_games`, and a `preset` reference on every tracked game. The product now has one concept instead: a single optional **Zone** (`zone: Option<Zone>`, absolute x/y/width/height) shared by all tracked games (see `CONTEXT.md`).

ADR 0001 chose to discard any config that doesn't match the current schema, because the project was pre-release with one user. That cost calculation changed: the user now has real tracked games, hotkeys and a drawn Custom region, and losing them for a vocabulary change is a worse trade than a small conversion function.

## Decision

`profile_store::load` tries, in order:

1. **New shape** — a config without a `[presets]` table is parsed as the Zone-based `Config` and returned as-is.
2. **Legacy conversion** — a config with a `[presets]` table is converted by the pure function `convert_legacy_config` (parsed TOML in, `Option<Config>` out; no filesystem access): `presets.custom` becomes `zone` with identical coordinates (absent means `None`); tracked games (exe path, display name, auto-apply), hotkeys and `taskbar_hide_mode` are kept; depths, `default_for_new_games` and each game's `preset` are dropped. The new shape is written back immediately, so the conversion runs once.
3. **Defaults** — corrupt TOML, or a legacy file that cannot be converted (e.g. the pre-#26 array-of-presets shape), is replaced with a default config, which has no Zone.

The legacy marker is checked before the new-shape parse because a legacy file would otherwise also deserialize as the new shape (serde ignores the unknown `presets` key) and silently lose its Custom region.

## Consequences

- Users who had only Left/Right presets (no Custom) end up with no Zone and must draw one; every apply path then does nothing and logs "no Zone is set". Left/Right have no equivalent and are not converted into a Zone.
- A game that was assigned Left or Right now gets the Zone like every other game.
- The legacy structs live in `profile_store.rs` and can be deleted once nobody is expected to hold a preset-era config.
- Downgrade hazard: once migrated, the file has no `[presets]` table, so an older (preset-era) GameFrame build reading it treats it as an unrecognised schema and resets to defaults, losing the games and the Zone. Migration is one-way.
- A file that cannot be converted, or is corrupt TOML, is copied to `config.toml.bak` (and logged) before being replaced with defaults, so nothing is lost without trace. If the migrated config cannot be written back, the error is logged and the converted config is still used; the conversion repeats on the next load.
- Reversibility: before the converted config overwrites a legacy file, the exact bytes `load` read are written to `config.toml.pre-zone.bak` (or the first free `config.toml.pre-zone.bak.N` when a different earlier copy exists). Backups are created with `create_new`, never overwritten, and skipped when an existing backup already holds identical bytes, so a write-back that keeps failing leaves one copy, not a growing pile. If the copy cannot be made, the write-back is skipped and the conversion is retried on the next load; a Settings or overlay save in between would still write the new shape over the original. A corrupt or unconvertible file goes the same way to `config.toml.bak[.N]` and is replaced with defaults only once that backup exists. If that backup fails, the file is left in place and defaults are used in memory, but a later save (Settings, overlay Zone Save or Clear) writes over it, and no copy of the original exists by then.

## Addendum (issue #49): the taskbar flag becomes global

This amends ADR 0002's per-game coupling. `TrackedGame.taskbar_autohide` is gone; `Config` has one `hide_taskbar_on_apply` (default true) that every apply path reads. Legacy conversion and a #48-shape config that still carries per-game flags both fold them on load: true if any game had it on, false if all had it off, true if there were no games; the per-game keys are dropped and the new shape is written back. A config that already has `hide_taskbar_on_apply` keeps it. ADR 0002's rest is unchanged: the armed-restore/refcount mechanics, and the manual taskbar hotkey staying independent of tracked games.
