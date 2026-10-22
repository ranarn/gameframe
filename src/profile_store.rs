//! Loads and saves GameFrame's config (Zone + hotkey bindings + tracked
//! games) as TOML, under a config directory injected by the caller. This
//! module never resolves `%APPDATA%` itself -- the caller passes in the
//! directory to use.

use std::path::{Path, PathBuf};

pub use crate::region_calc::Zone;

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

/// A process, identified by its executable path, that has been added to
/// GameFrame's per-game list -- see `CONTEXT.md`'s "Tracked game". Added
/// only through "Apply to Window": picking a window applies the Zone
/// immediately and adds it to this list if it isn't already present (see
/// [`upsert_tracked_game`]).
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
    /// Whether the Zone should be applied the moment the game launches,
    /// versus only on demand via the hotkey or tray menu.
    pub auto_apply: bool,
    /// Whether applying the Zone should also set the taskbar to auto-hide --
    /// see `docs/adr/0002-taskbar-autohide-coupled-to-preset-apply.md`.
    pub taskbar_autohide: bool,
}

impl TrackedGame {
    /// Builds a freshly-added tracked game with the specified defaults:
    /// auto-apply on, taskbar-auto-hide on.
    pub fn new_default(exe_path: PathBuf, display_name: String) -> Self {
        TrackedGame { exe_path, display_name, auto_apply: true, taskbar_autohide: true }
    }
}

/// Finds a tracked game whose `exe_path` matches, or appends a fresh
/// default-configured one ([`TrackedGame::new_default`]) if none matched --
/// the identity/dedupe rule behind the "Apply to Window" add flow (a game is
/// identified solely by its executable path; picking an already-tracked game
/// again must not create a duplicate entry, nor reset its existing settings,
/// nor overwrite its captured `display_name`).
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
pub fn upsert_tracked_game(games: &mut Vec<TrackedGame>, exe_path: PathBuf, display_name: String) -> usize {
    if let Some(pos) = games.iter().position(|g| g.exe_path == exe_path) {
        return pos;
    }
    games.push(TrackedGame::new_default(exe_path, display_name));
    games.len() - 1
}

/// Finds the tracked game (if any) whose `exe_path` matches -- the lookup
/// side of the hotkey/tray apply paths' "resolve against tracked games"
/// rule. Returns `None` for an untracked process: callers must treat that as
/// "no fallback".
///
/// Same case-sensitive-`PathBuf`-`Eq`-on-a-case-insensitive-filesystem
/// caveat as [`upsert_tracked_game`] -- accepted risk, see its doc comment.
pub fn find_tracked_game<'a>(games: &'a [TrackedGame], exe_path: &Path) -> Option<&'a TrackedGame> {
    games.iter().find(|g| g.exe_path == exe_path)
}

/// Which technique GameFrame uses to hide the taskbar (issue #42) -- applies
/// wherever a call site hides the taskbar (the borderless-toggle strip
/// direction, the tray "Apply to Window"/launch-detection apply paths, the
/// manual taskbar-autohide-toggle hotkey, and the zone-editor overlay's
/// force-on session), via `win32::plan_taskbar_hide`/`win32::hide_taskbar`.
///
/// `#[default]` is `FullyHidden` -- this issue's new default -- so
/// `#[serde(default)]` on [`Config::taskbar_hide_mode`] resolves a config
/// file written before this field existed to `FullyHidden` rather than
/// erroring, mirroring [`Config::tracked_games`]'s own backward-compat
/// pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum TaskbarHideMode {
    /// The taskbar is hidden via `ShowWindow(Shell_TrayWnd, SW_HIDE)` and
    /// stays hidden -- no Windows-native hover-near-the-edge reveal.
    #[default]
    FullyHidden,
    /// The taskbar is hidden via the native `SHAppBarMessage`/`ABS_AUTOHIDE`
    /// mechanism -- today's pre-#42 behavior, preserved unchanged: hovering
    /// the mouse near the screen edge reveals it.
    HoverReveal,
}

/// GameFrame's persisted config: the optional Zone, the two global hotkey
/// bindings, and the per-game tracked-games list.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Config {
    /// The single optional Zone. `None` until the user draws one.
    #[serde(default)]
    pub zone: Option<Zone>,
    pub hotkeys: HotkeyBindings,
    #[serde(default)]
    pub tracked_games: Vec<TrackedGame>,
    #[serde(default)]
    pub taskbar_hide_mode: TaskbarHideMode,
}

impl Default for Config {
    /// The starter config written on first run: no Zone, both hotkeys
    /// unbound, no tracked games yet, and the taskbar hidden fully (no
    /// hover-reveal) once the Zone is applied.
    fn default() -> Self {
        Config {
            zone: None,
            hotkeys: HotkeyBindings::default(),
            tracked_games: Vec::new(),
            taskbar_hide_mode: TaskbarHideMode::default(),
        }
    }
}

/// Reasons loading or saving the config can fail.
///
/// There is no `Malformed`/invalid-TOML variant: [`load`] never surfaces a
/// malformed or incompatible-schema config file as an error -- it discards
/// it and replaces it with defaults instead (see
/// `docs/adr/0001-preset-fixed-structure-no-migration.md` and
/// `docs/adr/0003-zone-config-migration.md`).
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

/// The preset-era config shape (`[presets]` table with `custom`, tracked
/// games carrying a `preset`). Only the fields that survive into the Zone
/// model are declared; everything else (depths, `default_for_new_games`, a
/// tracked game's `preset`) is ignored on purpose.
#[derive(serde::Deserialize)]
struct LegacyConfig {
    presets: LegacyPresets,
    hotkeys: HotkeyBindings,
    #[serde(default)]
    tracked_games: Vec<TrackedGame>,
    #[serde(default)]
    taskbar_hide_mode: TaskbarHideMode,
}

#[derive(serde::Deserialize)]
struct LegacyPresets {
    #[serde(default)]
    custom: Option<Zone>,
}

/// Converts a parsed preset-era config table into the Zone-based [`Config`]:
/// `presets.custom` becomes the Zone (identical coordinates; absent means
/// `None`), and tracked games (exe path, display name, auto-apply, taskbar
/// flag), hotkeys and hide mode are kept. Pure -- no filesystem access.
///
/// Returns `None` if the table isn't a convertible legacy config.
pub fn convert_legacy_config(table: toml::Table) -> Option<Config> {
    let legacy: LegacyConfig = table.try_into().ok()?;
    Some(Config {
        zone: legacy.presets.custom,
        hotkeys: legacy.hotkeys,
        tracked_games: legacy.tracked_games,
        taskbar_hide_mode: legacy.taskbar_hide_mode,
    })
}

/// Loads the config from `config_dir`, creating and persisting a default one
/// if none exists yet.
///
/// Order: a config already in the Zone shape loads as-is; a legacy
/// (preset-era, has a `[presets]` table) config is converted via
/// [`convert_legacy_config`] and the new shape is written back immediately;
/// anything else (corrupt TOML, unrecognised shape) is replaced with a
/// freshly written default config rather than returned as an error -- see
/// `docs/adr/0003-zone-config-migration.md`.
pub fn load(config_dir: &Path) -> Result<Config, ProfileStoreError> {
    let path = config_dir.join(CONFIG_FILE_NAME);

    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return write_default_config(config_dir);
        }
        Err(e) => return Err(ProfileStoreError::Io(e)),
    };

    let Ok(table) = contents.parse::<toml::Table>() else {
        back_up_unreadable_config(config_dir);
        return write_default_config(config_dir);
    };

    // A legacy file also deserializes as the new shape (serde ignores the
    // unknown `presets` key and would silently drop the Custom region), so
    // the legacy marker is checked first.
    if table.contains_key("presets") {
        return match convert_legacy_config(table) {
            Some(config) => {
                // The converted config is valid in memory; a failed
                // write-back (read-only file, AV lock) must not make every
                // caller fail. The original file is untouched and the
                // conversion simply repeats on the next load.
                if let Err(e) = save(config_dir, &config) {
                    eprintln!("GameFrame: could not write back migrated config: {e}");
                }
                Ok(config)
            }
            None => {
                back_up_unreadable_config(config_dir);
                write_default_config(config_dir)
            }
        };
    }

    match table.try_into::<Config>() {
        Ok(config) => Ok(config),
        Err(_) => {
            back_up_unreadable_config(config_dir);
            write_default_config(config_dir)
        }
    }
}

/// Copies the config file that is about to be replaced with defaults to
/// `config.toml.bak`, so an unconvertible or corrupt file is never lost
/// without trace. Best effort: failures are logged, not propagated.
fn back_up_unreadable_config(config_dir: &Path) {
    let path = config_dir.join(CONFIG_FILE_NAME);
    let backup = config_dir.join(format!("{CONFIG_FILE_NAME}.bak"));
    match std::fs::copy(&path, &backup) {
        Ok(_) => eprintln!(
            "GameFrame: config could not be read; reset to defaults, original kept at {}",
            backup.display()
        ),
        Err(e) => eprintln!("GameFrame: config could not be read and backup failed: {e}"),
    }
}

/// Builds a default [`Config`], persists it to `config_dir`, and returns it
/// -- the shared fallback for both "no config file yet" and "existing config
/// file doesn't match any known schema".
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

    fn write_config_file(config_dir: &Path, contents: &str) {
        std::fs::create_dir_all(config_dir).unwrap();
        std::fs::write(config_dir.join(CONFIG_FILE_NAME), contents).unwrap();
    }

    #[test]
    fn load_on_empty_directory_creates_default_config_file_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");

        let config = load(&config_dir).unwrap();

        assert_eq!(
            config,
            Config {
                zone: None,
                hotkeys: HotkeyBindings { toggle_borderless: None, toggle_taskbar_autohide: None },
                tracked_games: Vec::new(),
                taskbar_hide_mode: TaskbarHideMode::FullyHidden,
            }
        );
        assert!(config_dir.join(CONFIG_FILE_NAME).exists());
    }

    #[test]
    fn a_new_shape_config_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");
        let written = Config {
            zone: Some(Zone { x: 1, y: 2, width: 300, height: 400 }),
            hotkeys: HotkeyBindings {
                toggle_borderless: Some("Ctrl+Alt+B".to_string()),
                toggle_taskbar_autohide: Some("Ctrl+Alt+T".to_string()),
            },
            tracked_games: vec![TrackedGame {
                exe_path: PathBuf::from("C:\\Games\\thing.exe"),
                display_name: "Thing".to_string(),
                auto_apply: false,
                taskbar_autohide: true,
            }],
            taskbar_hide_mode: TaskbarHideMode::HoverReveal,
        };
        save(&config_dir, &written).unwrap();

        let loaded = load(&config_dir).unwrap();

        assert_eq!(loaded, written);
    }

    #[test]
    fn load_of_a_corrupt_config_file_resets_to_defaults_instead_of_erroring() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");
        write_config_file(&config_dir, "this is not { valid toml : : :");

        let config = load(&config_dir).unwrap();

        assert_eq!(config, Config::default());
        assert_eq!(config.zone, None);
        let backup = std::fs::read_to_string(config_dir.join("config.toml.bak")).unwrap();
        assert_eq!(backup, "this is not { valid toml : : :");
    }

    const LEGACY_WITH_CUSTOM: &str = r#"
        taskbar_hide_mode = "HoverReveal"

        [presets]
        left_depth_percent = 40
        right_depth_percent = 60
        default_for_new_games = "Right"

        [presets.custom]
        x = 10
        y = 20
        width = 200
        height = 100

        [hotkeys]
        toggle_borderless = "Ctrl+Alt+B"
        toggle_taskbar_autohide = "Ctrl+Alt+T"

        [[tracked_games]]
        exe_path = "C:\\Games\\thing.exe"
        display_name = "Thing"
        preset = "Custom"
        auto_apply = false
        taskbar_autohide = false

        [[tracked_games]]
        exe_path = "C:\\Games\\other.exe"
        display_name = "Other"
        preset = "Left"
        auto_apply = true
        taskbar_autohide = true
    "#;

    #[test]
    fn a_legacy_config_with_custom_loads_as_a_zone_keeping_everything_else() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");
        write_config_file(&config_dir, LEGACY_WITH_CUSTOM);

        let config = load(&config_dir).unwrap();

        assert_eq!(config.zone, Some(Zone { x: 10, y: 20, width: 200, height: 100 }));
        assert_eq!(config.hotkeys.toggle_borderless, Some("Ctrl+Alt+B".to_string()));
        assert_eq!(config.hotkeys.toggle_taskbar_autohide, Some("Ctrl+Alt+T".to_string()));
        assert_eq!(config.taskbar_hide_mode, TaskbarHideMode::HoverReveal);
        assert_eq!(
            config.tracked_games,
            vec![
                TrackedGame {
                    exe_path: PathBuf::from("C:\\Games\\thing.exe"),
                    display_name: "Thing".to_string(),
                    auto_apply: false,
                    taskbar_autohide: false,
                },
                TrackedGame {
                    exe_path: PathBuf::from("C:\\Games\\other.exe"),
                    display_name: "Other".to_string(),
                    auto_apply: true,
                    taskbar_autohide: true,
                },
            ]
        );
    }

    #[test]
    fn a_legacy_config_without_custom_gives_no_zone_and_keeps_games_and_hotkeys() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");
        write_config_file(
            &config_dir,
            r#"
            [presets]
            left_depth_percent = 40
            right_depth_percent = 60

            [hotkeys]
            toggle_borderless = "Ctrl+Alt+B"

            [[tracked_games]]
            exe_path = "C:\\Games\\thing.exe"
            display_name = "Thing"
            preset = "Right"
            auto_apply = true
            taskbar_autohide = true
            "#,
        );

        let config = load(&config_dir).unwrap();

        assert_eq!(config.zone, None);
        assert_eq!(config.hotkeys.toggle_borderless, Some("Ctrl+Alt+B".to_string()));
        assert_eq!(config.hotkeys.toggle_taskbar_autohide, None);
        assert_eq!(config.taskbar_hide_mode, TaskbarHideMode::FullyHidden);
        assert_eq!(config.tracked_games.len(), 1);
        assert_eq!(config.tracked_games[0].display_name, "Thing");
    }

    #[test]
    fn a_migrated_legacy_config_is_written_back_in_the_new_shape() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");
        write_config_file(&config_dir, LEGACY_WITH_CUSTOM);

        let migrated = load(&config_dir).unwrap();

        let on_disk = std::fs::read_to_string(config_dir.join(CONFIG_FILE_NAME)).unwrap();
        assert!(!on_disk.contains("presets"), "the legacy [presets] table must be gone: {on_disk}");
        assert!(!on_disk.contains("preset ="), "tracked games must no longer carry a preset: {on_disk}");
        assert!(on_disk.contains("[zone]"), "the zone must be written: {on_disk}");
        let second = load(&config_dir).unwrap();
        assert_eq!(second, migrated);
    }

    #[test]
    fn load_of_an_unconvertible_old_shape_config_resets_to_defaults() {
        // The pre-#26 shape (`presets` as an array of tagged variants) is
        // not convertible -- it is discarded and replaced with defaults.
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");
        write_config_file(
            &config_dir,
            r#"
            [[presets]]
            type = "LeftHalf"

            [hotkeys]
            toggle_borderless = "Ctrl+Alt+B"
            toggle_taskbar_autohide = "Ctrl+Alt+T"
            "#,
        );

        let config = load(&config_dir).unwrap();

        assert_eq!(config, Config::default());
        assert_eq!(load(&config_dir).unwrap(), Config::default());
        let backup = std::fs::read_to_string(config_dir.join("config.toml.bak")).unwrap();
        assert!(backup.contains("LeftHalf"));
    }

    #[test]
    fn load_of_a_config_file_missing_optional_keys_still_deserializes() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");
        write_config_file(
            &config_dir,
            r#"
            [hotkeys]
            toggle_borderless = "Ctrl+Alt+B"
            "#,
        );

        let config = load(&config_dir).unwrap();

        assert_eq!(config.zone, None);
        assert_eq!(config.tracked_games, Vec::new());
        assert_eq!(config.taskbar_hide_mode, TaskbarHideMode::FullyHidden);
        assert_eq!(config.hotkeys.toggle_borderless, Some("Ctrl+Alt+B".to_string()));
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
    fn tracked_game_new_default_has_auto_apply_and_taskbar_autohide_on() {
        let game = TrackedGame::new_default(PathBuf::from("C:\\Games\\thing.exe"), "Thing".to_string());

        assert_eq!(game.exe_path, PathBuf::from("C:\\Games\\thing.exe"));
        assert_eq!(game.display_name, "Thing");
        assert!(game.auto_apply);
        assert!(game.taskbar_autohide);
    }

    #[test]
    fn upsert_tracked_game_appends_a_new_default_entry_when_no_existing_match() {
        let mut games: Vec<TrackedGame> = Vec::new();

        let index = upsert_tracked_game(&mut games, PathBuf::from("C:\\Games\\thing.exe"), "Thing".to_string());

        assert_eq!(index, 0);
        assert_eq!(games.len(), 1);
        assert_eq!(games[0].display_name, "Thing");
        assert!(games[0].auto_apply);
    }

    #[test]
    fn upsert_tracked_game_reuses_the_existing_entry_for_a_matching_executable_path() {
        let mut games =
            vec![TrackedGame::new_default(PathBuf::from("C:\\Games\\thing.exe"), "Thing".to_string())];
        games[0].auto_apply = false;
        games[0].taskbar_autohide = false;

        let index = upsert_tracked_game(
            &mut games,
            PathBuf::from("C:\\Games\\thing.exe"),
            "Thing (different window title)".to_string(),
        );

        assert_eq!(index, 0);
        assert_eq!(games.len(), 1, "picking an already-tracked game again must not create a duplicate entry");
        // Display name is captured only at add-time.
        assert_eq!(games[0].display_name, "Thing");
        // Existing settings are preserved untouched, not reset to defaults.
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
        let games = vec![TrackedGame::new_default(PathBuf::from("C:\\Games\\thing.exe"), "Thing".to_string())];

        let found = find_tracked_game(&games, std::path::Path::new("C:\\Games\\thing.exe"));

        assert_eq!(found.map(|g| g.display_name.as_str()), Some("Thing"));
    }

    #[test]
    fn config_default_has_no_tracked_games() {
        assert_eq!(Config::default().tracked_games, Vec::new());
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
