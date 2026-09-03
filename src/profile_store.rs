//! Loads and saves GameFrame's config (presets + hotkey bindings) as TOML,
//! under a config directory injected by the caller. This module never
//! resolves `%APPDATA%` itself — the caller passes in the directory to use.

use crate::region_calc::Preset;
use std::path::{Path, PathBuf};

/// The config file name written inside the injected config directory.
pub const CONFIG_FILE_NAME: &str = "config.toml";

/// A global hotkey binding for a single action, e.g. `"Ctrl+Alt+B"`. `None`
/// means the action is currently unbound.
pub type HotkeyBinding = Option<String>;

/// The two global hotkey bindings GameFrame supports.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct HotkeyBindings {
    pub toggle_borderless: HotkeyBinding,
    pub toggle_taskbar_autohide: HotkeyBinding,
}

/// The default depth-from-edge percentage a freshly-created `Left`/`Right`
/// preset starts at.
pub const DEFAULT_DEPTH_PERCENT: u8 = 50;

/// The absolute-screen-coordinate bounds of the single optional Custom
/// preset. Kept as a plain struct (rather than reusing `Preset::Custom`
/// directly) so [`Presets`] can express "at most one Custom" as
/// `Option<CustomRegion>` without also having to guard against a `Custom`
/// value smuggling in a `Left`/`Right`/`HalfLeft`/`HalfRight` variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CustomRegion {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// GameFrame's preset set: exactly one of each of the four base presets
/// (`Left`, `Right`, `HalfLeft`, `HalfRight` -- `HalfLeft`/`HalfRight` carry
/// no adjustable data so they need no field of their own) plus at most one
/// `Custom`. This makes "always four base presets, at most one Custom, none
/// of them individually removable" a type-level invariant instead of a
/// UI-only or runtime-checked rule -- see `docs/adr/0001-preset-fixed-structure-no-migration.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Presets {
    pub left_depth_percent: u8,
    pub right_depth_percent: u8,
    pub custom: Option<CustomRegion>,
    /// The preset kind assigned to a newly-tracked game (see
    /// `CONTEXT.md`'s "Default preset (for new tracked games)") -- distinct
    /// from a tracked game's own "Active preset", which this field never
    /// changes once assigned. `#[serde(default = "default_preset_kind_for_new_games")]`
    /// resolves to `Right` for configs written before this field existed,
    /// matching the previously hardcoded behavior in `TrackedGame::new_default`.
    #[serde(default = "default_preset_kind_for_new_games")]
    pub default_for_new_games: PresetKind,
}

/// The `default_for_new_games` value used both as [`Presets`]'s
/// `Default` and as the serde fallback for configs predating this field --
/// `Right`, matching the previously hardcoded behavior of
/// `TrackedGame::new_default`.
fn default_preset_kind_for_new_games() -> PresetKind {
    PresetKind::Right
}

impl Default for Presets {
    fn default() -> Self {
        Presets {
            left_depth_percent: DEFAULT_DEPTH_PERCENT,
            right_depth_percent: DEFAULT_DEPTH_PERCENT,
            custom: None,
            default_for_new_games: default_preset_kind_for_new_games(),
        }
    }
}

/// A reference to one of GameFrame's five preset kinds -- never a copy of a
/// preset's data. A [`TrackedGame`]'s assigned preset is stored as one of
/// these (see [`CONTEXT.md`](../CONTEXT.md)'s "Active preset"), resolved
/// against the live [`Presets`] set at apply-time via [`Presets::resolve`],
/// so editing a base preset (or replacing Custom) immediately changes what
/// every game assigned to it applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PresetKind {
    Left,
    Right,
    HalfLeft,
    HalfRight,
    Custom,
}

impl PresetKind {
    /// All five preset kinds, in the same fixed presentation order as
    /// [`Presets::all`].
    pub const ALL: [PresetKind; 5] =
        [PresetKind::Left, PresetKind::Right, PresetKind::HalfLeft, PresetKind::HalfRight, PresetKind::Custom];
}

impl Presets {
    pub fn left(&self) -> Preset {
        Preset::Left { depth_percent: self.left_depth_percent }
    }

    pub fn right(&self) -> Preset {
        Preset::Right { depth_percent: self.right_depth_percent }
    }

    pub fn half_left(&self) -> Preset {
        Preset::HalfLeft
    }

    pub fn half_right(&self) -> Preset {
        Preset::HalfRight
    }

    pub fn custom(&self) -> Option<Preset> {
        self.custom.map(|c| Preset::Custom { x: c.x, y: c.y, width: c.width, height: c.height })
    }

    /// All presets currently in the set, in a fixed presentation order (the
    /// four base presets, then `Custom` if present).
    pub fn all(&self) -> Vec<Preset> {
        let mut presets = vec![self.left(), self.right(), self.half_left(), self.half_right()];
        if let Some(custom) = self.custom() {
            presets.push(custom);
        }
        presets
    }

    /// Resolves a [`PresetKind`] reference against this preset set into the
    /// actual [`Preset`] value to apply.
    ///
    /// Returns `None` only for [`PresetKind::Custom`] when no `Custom`
    /// preset is currently set (e.g. a tracked game was assigned `Custom`,
    /// then `Custom` was since cleared or replaced) -- every other kind
    /// always resolves. Callers must treat that `None` the same as "no
    /// tracked game found": no-op and log, never silently substitute a
    /// different preset.
    pub fn resolve(&self, kind: PresetKind) -> Option<Preset> {
        match kind {
            PresetKind::Left => Some(self.left()),
            PresetKind::Right => Some(self.right()),
            PresetKind::HalfLeft => Some(self.half_left()),
            PresetKind::HalfRight => Some(self.half_right()),
            PresetKind::Custom => self.custom(),
        }
    }
}

/// A process, identified by its executable path, that has been added to
/// GameFrame's per-game list -- see `CONTEXT.md`'s "Tracked game". Added
/// only through "Apply to Window": picking a window applies its
/// (default/existing) preset immediately and adds it to this list if it
/// isn't already present (see [`upsert_tracked_game`]).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TrackedGame {
    /// The full path to the game's executable -- this game's identity.
    /// Two windows belonging to processes with the same executable path are
    /// the same tracked game (see [`upsert_tracked_game`]).
    pub exe_path: PathBuf,
    /// Captured from the window's title the moment this game was first
    /// added -- never refreshed on a later re-apply, even if the window's
    /// title has since changed.
    pub display_name: String,
    /// The preset kind currently assigned to this game (see
    /// `CONTEXT.md`'s "Active preset"), resolved against the live
    /// [`Presets`] set at apply-time via [`Presets::resolve`].
    pub preset: PresetKind,
    /// Whether this game's preset should be applied the moment the game
    /// launches (issue #32), versus only on demand via the hotkey or tray
    /// menu.
    pub auto_apply: bool,
    /// Whether applying this game's preset should also set the taskbar to
    /// auto-hide -- see `docs/adr/0002-taskbar-autohide-coupled-to-preset-apply.md`.
    pub taskbar_autohide: bool,
}

impl TrackedGame {
    /// Builds a freshly-added tracked game with this issue's specified
    /// defaults: preset `default_preset` (the live
    /// `Presets::default_for_new_games` at the time of adding -- see
    /// `CONTEXT.md`'s "Default preset (for new tracked games)"), auto-apply
    /// on, taskbar-auto-hide on.
    pub fn new_default(exe_path: PathBuf, display_name: String, default_preset: PresetKind) -> Self {
        TrackedGame {
            exe_path,
            display_name,
            preset: default_preset,
            auto_apply: true,
            taskbar_autohide: true,
        }
    }
}

/// Finds a tracked game whose `exe_path` matches, or appends a fresh
/// default-configured one ([`TrackedGame::new_default`], seeded with
/// `default_preset`) if none matched -- the identity/dedupe rule behind the
/// "Apply to Window" add flow (a game is identified solely by its executable
/// path; picking an already-tracked game again must not create a duplicate
/// entry, nor reset its existing assignment/settings, nor overwrite its
/// captured `display_name`).
///
/// `default_preset` is the caller's current `Presets::default_for_new_games`
/// at the time of adding (see `CONTEXT.md`'s "Default preset (for new
/// tracked games)") -- only used when appending a new entry; it is never
/// applied to an existing match.
///
/// Returns the index of the matched-or-inserted entry in `games` so the
/// caller can read back whichever one now applies. Pure -- no OS/I-O
/// dependency -- so it's unit testable in complete isolation.
///
/// Matching uses `PathBuf`'s `Eq`, which is case-sensitive, on a filesystem
/// (NTFS) that itself is not -- an accepted risk rather than an oversight:
/// both apply paths resolve `exe_path` via the same OS API
/// (`QueryFullProcessImageNameW`, see `window_filter::process_exe_path`) for
/// a live process, which returns a consistent casing run-to-run, so two
/// differently-cased paths for the *same* game are not expected in
/// practice.
pub fn upsert_tracked_game(
    games: &mut Vec<TrackedGame>,
    exe_path: PathBuf,
    display_name: String,
    default_preset: PresetKind,
) -> usize {
    if let Some(pos) = games.iter().position(|g| g.exe_path == exe_path) {
        return pos;
    }
    games.push(TrackedGame::new_default(exe_path, display_name, default_preset));
    games.len() - 1
}

/// Finds the tracked game (if any) whose `exe_path` matches -- the lookup
/// side of the hotkey/tray apply paths' "resolve against tracked games"
/// rule. Returns `None` for an untracked process: callers must treat that as
/// "no fallback", per this issue's acceptance criteria.
///
/// Same case-sensitive-`PathBuf`-`Eq`-on-a-case-insensitive-filesystem
/// caveat as [`upsert_tracked_game`] -- accepted risk, see its doc comment.
pub fn find_tracked_game<'a>(games: &'a [TrackedGame], exe_path: &Path) -> Option<&'a TrackedGame> {
    games.iter().find(|g| g.exe_path == exe_path)
}

/// Selects every tracked game currently assigned to `kind` -- the
/// cascade-selection decision behind issue #33's live re-apply: editing a
/// *shared* value (a base preset's depth-from-edge, or the Custom zone)
/// affects every tracked game assigned to that preset, unlike reassigning a
/// single game's own dropdown, which only affects that one game.
///
/// Pure -- no OS/I-O dependency -- so it's unit testable in complete
/// isolation from the live-window resolution/apply step that consumes this
/// selection.
pub fn tracked_games_using_preset(tracked_games: &[TrackedGame], kind: PresetKind) -> Vec<&TrackedGame> {
    tracked_games.iter().filter(|g| g.preset == kind).collect()
}

/// GameFrame's persisted config: the preset set, the two global hotkey
/// bindings, and the per-game tracked-games list.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Config {
    pub presets: Presets,
    pub hotkeys: HotkeyBindings,
    #[serde(default)]
    pub tracked_games: Vec<TrackedGame>,
}

impl Default for Config {
    /// The starter config written on first run: default-depth Left/Right, no
    /// Custom, both hotkeys unbound, and no tracked games yet.
    fn default() -> Self {
        Config { presets: Presets::default(), hotkeys: HotkeyBindings::default(), tracked_games: Vec::new() }
    }
}

/// Reasons loading or saving the config can fail.
///
/// There is no `Malformed`/invalid-TOML variant: since the no-migration
/// reshape (issue #26), [`load`] never surfaces a malformed or
/// incompatible-schema config file as an error -- it discards it and
/// replaces it with defaults instead (see
/// `docs/adr/0001-preset-fixed-structure-no-migration.md`).
#[derive(Debug)]
pub enum ProfileStoreError {
    /// A filesystem operation (read, write, or directory creation) failed.
    Io(std::io::Error),
    /// The in-memory config could not be serialized to TOML (should not
    /// normally happen for a valid `Config`).
    Serialize(toml::ser::Error),
}

impl std::fmt::Display for ProfileStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProfileStoreError::Io(e) => write!(f, "config I/O error: {e}"),
            ProfileStoreError::Serialize(e) => write!(f, "failed to serialize config: {e}"),
        }
    }
}

impl std::error::Error for ProfileStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ProfileStoreError::Io(e) => Some(e),
            ProfileStoreError::Serialize(e) => Some(e),
        }
    }
}

/// Loads the config from `config_dir`, creating and persisting a default one
/// if none exists yet.
///
/// No migration path: if the file exists but its contents don't match the
/// current `Config` schema (whether that's an old pre-reshape shape, or
/// otherwise-corrupt TOML), it is discarded and replaced with a freshly
/// written default config rather than returned as an error -- see
/// `docs/adr/0001-preset-fixed-structure-no-migration.md`. GameFrame is
/// pre-release with a single user, so the cost of a migration outweighs its
/// value here.
pub fn load(config_dir: &Path) -> Result<Config, ProfileStoreError> {
    let path = config_dir.join(CONFIG_FILE_NAME);

    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return write_default_config(config_dir);
        }
        Err(e) => return Err(ProfileStoreError::Io(e)),
    };

    match toml::from_str(&contents) {
        Ok(config) => Ok(config),
        Err(_) => write_default_config(config_dir),
    }
}

/// Builds a default [`Config`], persists it to `config_dir`, and returns it
/// -- the shared fallback for both "no config file yet" and "existing config
/// file doesn't match the current schema".
fn write_default_config(config_dir: &Path) -> Result<Config, ProfileStoreError> {
    let default_config = Config::default();
    save(config_dir, &default_config)?;
    Ok(default_config)
}

pub fn save(config_dir: &Path, config: &Config) -> Result<(), ProfileStoreError> {
    std::fs::create_dir_all(config_dir).map_err(ProfileStoreError::Io)?;

    let contents = toml::to_string_pretty(config).map_err(ProfileStoreError::Serialize)?;

    // Write to a temp file in the same directory first, then rename into
    // place, so a crash or power loss mid-write can't leave a
    // truncated/corrupt config file behind.
    let final_path = config_dir.join(CONFIG_FILE_NAME);
    let temp_path = config_dir.join(format!("{CONFIG_FILE_NAME}.tmp"));

    std::fs::write(&temp_path, contents).map_err(ProfileStoreError::Io)?;
    std::fs::rename(&temp_path, &final_path).map_err(ProfileStoreError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_on_empty_directory_creates_default_config_file_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");

        let config = load(&config_dir).unwrap();

        assert_eq!(
            config,
            Config {
                presets: Presets {
                    left_depth_percent: DEFAULT_DEPTH_PERCENT,
                    right_depth_percent: DEFAULT_DEPTH_PERCENT,
                    custom: None,
                    default_for_new_games: PresetKind::Right,
                },
                hotkeys: HotkeyBindings { toggle_borderless: None, toggle_taskbar_autohide: None },
                tracked_games: Vec::new(),
            }
        );
        assert!(config_dir.join(CONFIG_FILE_NAME).exists());
    }

    #[test]
    fn load_reads_back_an_existing_config_file_on_a_later_run() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");
        let written = Config {
            presets: Presets {
                left_depth_percent: 30,
                right_depth_percent: 70,
                custom: Some(CustomRegion { x: 1, y: 2, width: 300, height: 400 }),
                default_for_new_games: PresetKind::HalfLeft,
            },
            hotkeys: HotkeyBindings {
                toggle_borderless: Some("Ctrl+Alt+B".to_string()),
                toggle_taskbar_autohide: Some("Ctrl+Alt+T".to_string()),
            },
            tracked_games: Vec::new(),
        };
        save(&config_dir, &written).unwrap();

        let loaded = load(&config_dir).unwrap();

        assert_eq!(loaded, written);
    }

    #[test]
    fn load_of_a_corrupt_config_file_resets_to_defaults_instead_of_erroring() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join(CONFIG_FILE_NAME), "this is not { valid toml : : :").unwrap();

        let config = load(&config_dir).unwrap();

        assert_eq!(config, Config::default());
    }

    #[test]
    fn load_of_an_old_shape_config_file_resets_to_defaults_with_no_migration() {
        // The pre-reshape config shape: `presets` was a free-form array
        // including the now-removed `FullScreen` variant and `LeftHalf`/
        // `RightHalf` instead of `Left`/`Right`. This no longer deserializes
        // into the current `Config` schema at all, and per
        // docs/adr/0001-preset-fixed-structure-no-migration.md there is no
        // migration path -- it is discarded and replaced with defaults.
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");
        std::fs::create_dir_all(&config_dir).unwrap();
        let old_shape_toml = r#"
            [[presets]]
            type = "LeftHalf"

            [[presets]]
            type = "FullScreen"

            [hotkeys]
            toggle_borderless = "Ctrl+Alt+B"
            toggle_taskbar_autohide = "Ctrl+Alt+T"
        "#;
        std::fs::write(config_dir.join(CONFIG_FILE_NAME), old_shape_toml).unwrap();

        let config = load(&config_dir).unwrap();

        assert_eq!(config, Config::default());
        // The reset is also persisted, so the next load doesn't need to
        // rediscover the same incompatible file.
        let reloaded = load(&config_dir).unwrap();
        assert_eq!(reloaded, Config::default());
    }

    #[test]
    fn load_of_a_current_shape_config_file_missing_only_the_tracked_games_key_still_deserializes() {
        // A config saved between #26 (current `presets`/`hotkeys` shape
        // landed) and #31 (this `tracked_games` field being added) landing --
        // distinct from `load_of_an_old_shape_config_file_resets_to_defaults_with_no_migration`
        // above, which covers a pre-#26 shape that doesn't deserialize into
        // `Config` at all. Here the rest of the shape is valid and current;
        // only the new `tracked_games` key is absent. `#[serde(default)]`
        // on `Config::tracked_games` exists specifically so this succeeds
        // with the rest of the config preserved, rather than falling into
        // the "incompatible schema, reset everything" path.
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");
        std::fs::create_dir_all(&config_dir).unwrap();
        let toml_missing_tracked_games = r#"
            [presets]
            left_depth_percent = 40
            right_depth_percent = 60
            custom = { x = 10, y = 10, width = 200, height = 100 }

            [hotkeys]
            toggle_borderless = "Ctrl+Alt+B"
            toggle_taskbar_autohide = "Ctrl+Alt+T"
        "#;
        std::fs::write(config_dir.join(CONFIG_FILE_NAME), toml_missing_tracked_games).unwrap();

        let config = load(&config_dir).unwrap();

        assert_eq!(config.presets.left_depth_percent, 40);
        assert_eq!(config.presets.right_depth_percent, 60);
        assert_eq!(config.presets.custom, Some(CustomRegion { x: 10, y: 10, width: 200, height: 100 }));
        assert_eq!(config.hotkeys.toggle_borderless, Some("Ctrl+Alt+B".to_string()));
        assert_eq!(config.hotkeys.toggle_taskbar_autohide, Some("Ctrl+Alt+T".to_string()));
        assert_eq!(config.tracked_games, Vec::new(), "a missing tracked_games key must default to empty, not error");
    }

    #[test]
    fn load_of_a_current_shape_config_file_missing_only_the_default_for_new_games_key_still_deserializes() {
        // A config saved before this issue's `default_for_new_games` field
        // was added -- `#[serde(default = "default_preset_kind_for_new_games")]`
        // exists specifically so this still deserializes (rather than
        // resetting to defaults), resolving to `Right` to match the
        // previously hardcoded `TrackedGame::new_default` behavior.
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");
        std::fs::create_dir_all(&config_dir).unwrap();
        let toml_missing_default_for_new_games = r#"
            [presets]
            left_depth_percent = 40
            right_depth_percent = 60
            custom = { x = 10, y = 10, width = 200, height = 100 }

            [hotkeys]
            toggle_borderless = "Ctrl+Alt+B"
            toggle_taskbar_autohide = "Ctrl+Alt+T"
        "#;
        std::fs::write(config_dir.join(CONFIG_FILE_NAME), toml_missing_default_for_new_games).unwrap();

        let config = load(&config_dir).unwrap();

        assert_eq!(
            config.presets.default_for_new_games,
            PresetKind::Right,
            "a missing default_for_new_games key must default to Right, not error"
        );
    }

    #[test]
    fn saving_a_changed_config_overwrites_the_previous_file_contents() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");
        let mut config = load(&config_dir).unwrap(); // creates the default file
        assert_eq!(config.hotkeys.toggle_borderless, None);

        config.hotkeys.toggle_borderless = Some("Ctrl+Alt+B".to_string());
        save(&config_dir, &config).unwrap();

        let reloaded = load(&config_dir).unwrap();
        assert_eq!(reloaded.hotkeys.toggle_borderless, Some("Ctrl+Alt+B".to_string()));
    }

    #[test]
    fn different_config_dirs_persist_independently() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir_a = dir.path().join("A");
        let config_dir_b = dir.path().join("B");

        let mut config_a = load(&config_dir_a).unwrap();
        config_a.hotkeys.toggle_taskbar_autohide = Some("Ctrl+Alt+D".to_string());
        save(&config_dir_a, &config_a).unwrap();

        let config_b = load(&config_dir_b).unwrap();

        assert_eq!(config_b.hotkeys.toggle_taskbar_autohide, None);
    }

    #[test]
    fn presets_all_lists_the_four_base_presets_in_fixed_order_with_no_custom_by_default() {
        let presets = Presets::default();

        let all = presets.all();

        assert_eq!(
            all,
            vec![
                Preset::Left { depth_percent: DEFAULT_DEPTH_PERCENT },
                Preset::Right { depth_percent: DEFAULT_DEPTH_PERCENT },
                Preset::HalfLeft,
                Preset::HalfRight,
            ]
        );
    }

    #[test]
    fn presets_all_appends_custom_when_present() {
        let presets = Presets {
            left_depth_percent: 20,
            right_depth_percent: 80,
            custom: Some(CustomRegion { x: 1, y: 2, width: 3, height: 4 }),
            default_for_new_games: PresetKind::Right,
        };

        let all = presets.all();

        assert_eq!(
            all,
            vec![
                Preset::Left { depth_percent: 20 },
                Preset::Right { depth_percent: 80 },
                Preset::HalfLeft,
                Preset::HalfRight,
                Preset::Custom { x: 1, y: 2, width: 3, height: 4 },
            ]
        );
    }

    #[test]
    fn presets_resolve_returns_the_matching_preset_for_each_base_kind() {
        let presets = Presets {
            left_depth_percent: 20,
            right_depth_percent: 80,
            custom: None,
            default_for_new_games: PresetKind::Right,
        };

        assert_eq!(presets.resolve(PresetKind::Left), Some(Preset::Left { depth_percent: 20 }));
        assert_eq!(presets.resolve(PresetKind::Right), Some(Preset::Right { depth_percent: 80 }));
        assert_eq!(presets.resolve(PresetKind::HalfLeft), Some(Preset::HalfLeft));
        assert_eq!(presets.resolve(PresetKind::HalfRight), Some(Preset::HalfRight));
    }

    #[test]
    fn presets_resolve_returns_the_custom_preset_when_one_is_set() {
        let presets = Presets {
            left_depth_percent: 20,
            right_depth_percent: 80,
            custom: Some(CustomRegion { x: 1, y: 2, width: 3, height: 4 }),
            default_for_new_games: PresetKind::Right,
        };

        assert_eq!(presets.resolve(PresetKind::Custom), Some(Preset::Custom { x: 1, y: 2, width: 3, height: 4 }));
    }

    #[test]
    fn presets_resolve_returns_none_for_custom_when_none_is_currently_set() {
        // A game assigned to Custom after Custom was since cleared/replaced
        // with None -- callers must treat this the same as "untracked" for
        // that one apply, not silently fall back to a different preset.
        let presets = Presets {
            left_depth_percent: 20,
            right_depth_percent: 80,
            custom: None,
            default_for_new_games: PresetKind::Right,
        };

        assert_eq!(presets.resolve(PresetKind::Custom), None);
    }

    #[test]
    fn tracked_game_new_default_uses_the_given_default_preset_auto_apply_and_taskbar_autohide_on() {
        let game = TrackedGame::new_default(
            PathBuf::from("C:\\Games\\thing.exe"),
            "Thing".to_string(),
            PresetKind::Right,
        );

        assert_eq!(game.exe_path, PathBuf::from("C:\\Games\\thing.exe"));
        assert_eq!(game.display_name, "Thing");
        assert_eq!(game.preset, PresetKind::Right);
        assert!(game.auto_apply);
        assert!(game.taskbar_autohide);
    }

    #[test]
    fn tracked_game_new_default_uses_whichever_default_preset_it_is_given() {
        // Confirms the assigned preset tracks the passed-in parameter, not a
        // hardcoded kind -- see CONTEXT.md's "Default preset (for new
        // tracked games)".
        let game = TrackedGame::new_default(
            PathBuf::from("C:\\Games\\thing.exe"),
            "Thing".to_string(),
            PresetKind::HalfLeft,
        );

        assert_eq!(game.preset, PresetKind::HalfLeft);
    }

    #[test]
    fn upsert_tracked_game_appends_a_new_default_entry_when_no_existing_match() {
        let mut games: Vec<TrackedGame> = Vec::new();

        let index = upsert_tracked_game(
            &mut games,
            PathBuf::from("C:\\Games\\thing.exe"),
            "Thing".to_string(),
            PresetKind::Right,
        );

        assert_eq!(index, 0);
        assert_eq!(games.len(), 1);
        assert_eq!(games[0].display_name, "Thing");
        assert_eq!(games[0].preset, PresetKind::Right);
    }

    #[test]
    fn upsert_tracked_game_appends_a_new_entry_using_whichever_default_preset_it_is_given() {
        // A newly-tracked game (e.g. via "Apply to Window") must be seeded
        // from the caller's current `Presets::default_for_new_games`, not a
        // hardcoded kind -- see CONTEXT.md's "Default preset (for new
        // tracked games)".
        let mut games: Vec<TrackedGame> = Vec::new();

        let index = upsert_tracked_game(
            &mut games,
            PathBuf::from("C:\\Games\\thing.exe"),
            "Thing".to_string(),
            PresetKind::HalfRight,
        );

        assert_eq!(index, 0);
        assert_eq!(games[0].preset, PresetKind::HalfRight);
    }

    #[test]
    fn upsert_tracked_game_reuses_the_existing_entry_for_a_matching_executable_path() {
        let mut games = vec![TrackedGame::new_default(
            PathBuf::from("C:\\Games\\thing.exe"),
            "Thing".to_string(),
            PresetKind::Right,
        )];
        games[0].preset = PresetKind::Custom;
        games[0].auto_apply = false;
        games[0].taskbar_autohide = false;

        let index = upsert_tracked_game(
            &mut games,
            PathBuf::from("C:\\Games\\thing.exe"),
            "Thing (different window title)".to_string(),
            PresetKind::Right,
        );

        assert_eq!(index, 0);
        assert_eq!(games.len(), 1, "picking an already-tracked game again must not create a duplicate entry");
        // Display name is captured only at add-time -- a later re-pick with
        // a different window title must not overwrite it.
        assert_eq!(games[0].display_name, "Thing");
        // Existing assignment/settings are preserved untouched, not reset
        // back to defaults.
        assert_eq!(games[0].preset, PresetKind::Custom);
        assert!(!games[0].auto_apply);
        assert!(!games[0].taskbar_autohide);
    }

    #[test]
    fn find_tracked_game_returns_none_when_no_game_matches() {
        let games: Vec<TrackedGame> = Vec::new();

        assert!(find_tracked_game(&games, std::path::Path::new("C:\\Games\\thing.exe")).is_none());
    }

    #[test]
    fn find_tracked_game_returns_the_matching_entry() {
        let games = vec![TrackedGame::new_default(
            PathBuf::from("C:\\Games\\thing.exe"),
            "Thing".to_string(),
            PresetKind::Right,
        )];

        let found = find_tracked_game(&games, std::path::Path::new("C:\\Games\\thing.exe"));

        assert_eq!(found.map(|g| g.display_name.as_str()), Some("Thing"));
    }

    #[test]
    fn tracked_games_using_preset_returns_empty_for_an_empty_list() {
        let games: Vec<TrackedGame> = Vec::new();

        let matches = tracked_games_using_preset(&games, PresetKind::Left);

        assert!(matches.is_empty());
    }

    #[test]
    fn tracked_games_using_preset_returns_empty_when_no_game_uses_the_preset() {
        let mut game =
            TrackedGame::new_default(PathBuf::from("C:\\Games\\thing.exe"), "Thing".to_string(), PresetKind::Right);
        game.preset = PresetKind::Right;
        let games = vec![game];

        let matches = tracked_games_using_preset(&games, PresetKind::Left);

        assert!(matches.is_empty());
    }

    #[test]
    fn tracked_games_using_preset_returns_the_single_matching_game() {
        let mut game =
            TrackedGame::new_default(PathBuf::from("C:\\Games\\thing.exe"), "Thing".to_string(), PresetKind::Right);
        game.preset = PresetKind::Left;
        let games = vec![game];

        let matches = tracked_games_using_preset(&games, PresetKind::Left);

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].display_name, "Thing");
    }

    #[test]
    fn tracked_games_using_preset_returns_every_matching_game_and_none_else() {
        let mut a =
            TrackedGame::new_default(PathBuf::from("C:\\Games\\a.exe"), "A".to_string(), PresetKind::Right);
        a.preset = PresetKind::Custom;
        let mut b =
            TrackedGame::new_default(PathBuf::from("C:\\Games\\b.exe"), "B".to_string(), PresetKind::Right);
        b.preset = PresetKind::Left;
        let mut c =
            TrackedGame::new_default(PathBuf::from("C:\\Games\\c.exe"), "C".to_string(), PresetKind::Right);
        c.preset = PresetKind::Custom;
        let games = vec![a, b, c];

        let matches = tracked_games_using_preset(&games, PresetKind::Custom);

        let names: Vec<&str> = matches.iter().map(|g| g.display_name.as_str()).collect();
        assert_eq!(names, vec!["A", "C"]);
    }

    #[test]
    fn config_default_has_no_tracked_games() {
        assert_eq!(Config::default().tracked_games, Vec::new());
    }

    #[test]
    fn load_reads_back_tracked_games_persisted_in_a_saved_config() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");
        let mut written = Config::default();
        written.tracked_games.push(TrackedGame {
            exe_path: PathBuf::from("C:\\Games\\thing.exe"),
            display_name: "Thing".to_string(),
            preset: PresetKind::HalfLeft,
            auto_apply: false,
            taskbar_autohide: true,
        });
        save(&config_dir, &written).unwrap();

        let loaded = load(&config_dir).unwrap();

        assert_eq!(loaded, written);
    }

    #[test]
    fn profile_store_error_implements_display_and_std_error() {
        fn assert_is_std_error<E: std::error::Error>(_: &E) {}

        let io_err = ProfileStoreError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "config file not found",
        ));
        let serialize_err = match toml::to_string(&5i32) {
            Err(e) => ProfileStoreError::Serialize(e),
            Ok(_) => panic!("expected serializing a bare integer to fail"),
        };

        assert_is_std_error(&io_err);
        assert_is_std_error(&serialize_err);

        assert_eq!(io_err.to_string(), "config I/O error: config file not found");
        assert!(serialize_err.to_string().starts_with("failed to serialize config: "));
    }
}
