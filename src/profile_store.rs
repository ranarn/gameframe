//! Loads and saves GameFrame's config (Zone + hotkey bindings + tracked
//! games) as TOML, under a config directory injected by the caller. This
//! module never resolves `%APPDATA%` itself -- the caller passes in the
//! directory to use.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub use crate::cursor_style::CursorSettings;
pub use crate::region_calc::Zone;

/// The config file name written inside the injected config directory.
pub const CONFIG_FILE_NAME: &str = "config.toml";

/// A global hotkey binding for a single action, e.g. `"Ctrl+Alt+B"`. `None`
/// means the action is currently unbound.
pub type HotkeyBinding = Option<String>;

/// The global hotkey bindings GameFrame supports.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct HotkeyBindings {
    pub toggle_borderless: HotkeyBinding,
    pub toggle_taskbar_autohide: HotkeyBinding,
    /// Turns the game cursor on/off (see [`crate::cursor_style`]). Optional in
    /// the file, so configs without it still load.
    #[serde(default)]
    pub toggle_game_cursor: HotkeyBinding,
}

/// A process, identified by its executable path, that has been added to
/// GameFrame's per-game list. Added
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
    /// Whether the game cursor is shown while this game is in the
    /// foreground (when the cursor's scope is "tracked games"). Defaults to
    /// on, including for games tracked before the game cursor existed.
    #[serde(default = "default_true")]
    pub game_cursor: bool,
}

fn default_true() -> bool {
    true
}

impl TrackedGame {
    /// Builds a freshly-added tracked game with the specified defaults:
    /// auto-apply on, game cursor on.
    pub fn new_default(exe_path: PathBuf, display_name: String) -> Self {
        TrackedGame {
            exe_path,
            display_name,
            auto_apply: true,
            game_cursor: true,
        }
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
pub fn upsert_tracked_game(
    games: &mut Vec<TrackedGame>,
    exe_path: PathBuf,
    display_name: String,
) -> usize {
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

/// Which technique GameFrame uses to hide the taskbar -- applies
/// wherever a call site hides the taskbar (the borderless-toggle strip
/// direction, the tray "Apply to Window"/launch-detection apply paths, the
/// manual taskbar-autohide-toggle hotkey, and the zone-editor overlay's
/// force-on session), via `win32::plan_taskbar_hide`/`win32::hide_taskbar`.
///
/// `#[default]` is `FullyHidden`, so `#[serde(default)]` on
/// [`Config::taskbar_hide_mode`] resolves a config file without this field to
/// `FullyHidden` rather than erroring, mirroring [`Config::tracked_games`]'s
/// own backward-compat pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum TaskbarHideMode {
    /// The taskbar is hidden via `ShowWindow(Shell_TrayWnd, SW_HIDE)` and
    /// stays hidden -- no Windows-native hover-near-the-edge reveal.
    #[default]
    FullyHidden,
    /// The taskbar is hidden via the native `SHAppBarMessage`/`ABS_AUTOHIDE`
    /// mechanism: hovering the mouse near the screen edge reveals it.
    HoverReveal,
}

/// How GameFrame reacts when its startup update check finds a newer release.
/// Serialized as `"auto"` / `"ask"`; a config without the field means `Auto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdateMode {
    /// Install the update right away, silently.
    #[default]
    Auto,
    /// Add an "Update to vX.Y.Z" tray item and install when it is chosen.
    Ask,
}

/// GameFrame's persisted config: the optional Zone, the three global hotkey
/// bindings, the tracked-games list, the taskbar settings and the game cursor
/// settings.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Config {
    /// The single optional Zone. `None` until the user draws one.
    #[serde(default)]
    pub zone: Option<Zone>,
    /// All unbound when the `[hotkeys]` table is missing.
    #[serde(default)]
    pub hotkeys: HotkeyBindings,
    #[serde(default)]
    pub tracked_games: Vec<TrackedGame>,
    #[serde(default)]
    pub taskbar_hide_mode: TaskbarHideMode,
    /// Whether applying the Zone also hides the taskbar (using
    /// `taskbar_hide_mode`). Off by default.
    #[serde(default = "default_hide_taskbar_on_apply")]
    pub hide_taskbar_on_apply: bool,
    /// The game cursor's settings (config table `[cursor]`).
    #[serde(default)]
    pub cursor: CursorSettings,
    /// What the startup update check does with an available update.
    #[serde(default)]
    pub update_mode: UpdateMode,
    /// Whether the main-thread hang watchdog runs and logs UI stalls
    /// (crate::watchdog). On by default; a config without the field means on.
    #[serde(default = "default_log_ui_hangs")]
    pub log_ui_hangs: bool,
}

fn default_hide_taskbar_on_apply() -> bool {
    false
}

fn default_log_ui_hangs() -> bool {
    true
}

impl Default for Config {
    /// The starter config written on first run: no Zone, all hotkeys
    /// unbound, no tracked games yet, and the taskbar left alone when the
    /// Zone is applied (`hide_taskbar_on_apply` off).
    fn default() -> Self {
        Config {
            zone: None,
            hotkeys: HotkeyBindings::default(),
            tracked_games: Vec::new(),
            taskbar_hide_mode: TaskbarHideMode::default(),
            hide_taskbar_on_apply: default_hide_taskbar_on_apply(),
            cursor: CursorSettings::default(),
            update_mode: UpdateMode::default(),
            log_ui_hangs: default_log_ui_hangs(),
        }
    }
}

/// Whether startup should open the zone editor once: true exactly when the
/// config has no Zone (first run, or a corrupt config reset to defaults).
pub fn should_prompt_for_zone(config: &Config) -> bool {
    config.zone.is_none()
}

/// Reasons loading or saving the config can fail.
///
/// There is no `Malformed`/invalid-TOML variant: [`load`] never surfaces a
/// malformed or incompatible-schema config file as an error -- it discards
/// it and replaces it with defaults instead.
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
    #[serde(default = "default_hide_taskbar_on_apply")]
    hide_taskbar_on_apply: bool,
}

#[derive(serde::Deserialize)]
struct LegacyPresets {
    #[serde(default)]
    custom: Option<Zone>,
}

/// Key of the legacy per-game taskbar flag, which is folded into the global
/// [`Config::hide_taskbar_on_apply`] when an old config is loaded.
const PER_GAME_TASKBAR_KEY: &str = "taskbar_autohide";

/// Folds per-game `taskbar_autohide` flags (present in older
/// configs) into the global `hide_taskbar_on_apply` key: true if any game had
/// it on, false if every game had it off. The no-games case does reach this
/// fold but changes nothing (the value then comes from
/// `default_hide_taskbar_on_apply`). The per-game keys are removed. A table
/// that already carries `hide_taskbar_on_apply` keeps it.
/// Returns whether the table changed. Pure -- no filesystem access.
fn fold_per_game_taskbar_flags(table: &mut toml::Table) -> bool {
    let mut flags = Vec::new();
    let mut changed = false;
    if let Some(toml::Value::Array(games)) = table.get_mut("tracked_games") {
        for game in games.iter_mut() {
            if let Some(entry) = game.as_table_mut()
                && let Some(value) = entry.remove(PER_GAME_TASKBAR_KEY)
            {
                changed = true;
                // A malformed (non-bool) flag counts as the old default, on.
                flags.push(value.as_bool().unwrap_or(true));
            }
        }
    }
    if changed && !table.contains_key("hide_taskbar_on_apply") {
        let global = flags.iter().any(|&f| f);
        table.insert(
            "hide_taskbar_on_apply".to_string(),
            toml::Value::Boolean(global),
        );
    }
    changed
}

/// Converts a parsed preset-era config table into the Zone-based [`Config`]:
/// `presets.custom` becomes the Zone (identical coordinates; absent means
/// `None`), and tracked games (exe path, display name, auto-apply), hotkeys
/// and hide mode are kept; per-game taskbar flags fold into the global
/// `hide_taskbar_on_apply` (see [`fold_per_game_taskbar_flags`]). Pure -- no
/// filesystem access.
///
/// Returns `None` if the table isn't a convertible legacy config.
pub fn convert_legacy_config(mut table: toml::Table) -> Option<Config> {
    fold_per_game_taskbar_flags(&mut table);
    let legacy: LegacyConfig = table.try_into().ok()?;
    Some(Config {
        zone: legacy.presets.custom,
        hotkeys: legacy.hotkeys,
        tracked_games: legacy.tracked_games,
        taskbar_hide_mode: legacy.taskbar_hide_mode,
        hide_taskbar_on_apply: legacy.hide_taskbar_on_apply,
        cursor: CursorSettings::default(),
        update_mode: UpdateMode::default(),
        log_ui_hangs: default_log_ui_hangs(),
    })
}

/// Loads the config from `config_dir`, creating and persisting a default one
/// if none exists yet.
///
/// Order: a config already in the Zone shape loads as-is; a legacy
/// (preset-era, has a `[presets]` table) config is converted via
/// [`convert_legacy_config`] and the new shape is written back immediately; a
/// config still carrying per-game `taskbar_autohide` flags has them
/// folded into the global setting ([`fold_per_game_taskbar_flags`]) and is
/// written back too; anything else (corrupt TOML, unrecognised shape) is
/// replaced with a freshly written default config rather than returned as an
/// error.
///
/// The result is cached in memory per config directory: the first call reads
/// the file, later calls are served from memory, and [`save`] refreshes the
/// cache with what it wrote. Edits made to the file by hand while GameFrame is
/// running are therefore not picked up until the next start.
pub fn load(config_dir: &Path) -> Result<Config, ProfileStoreError> {
    if let Some(config) = lock_cache().get(config_dir) {
        return Ok(config.clone());
    }
    // The disk read runs without the lock. A save that landed meanwhile has
    // already filled the cache and wins over the older config read here.
    // Errors are not cached, so a transient read failure does not stick.
    let config = load_from_disk(config_dir)?;
    Ok(lock_cache()
        .entry(config_dir.to_path_buf())
        .or_insert(config)
        .clone())
}

/// The in-memory copy of each config directory's config, see [`load`].
static CACHE: std::sync::LazyLock<std::sync::Mutex<HashMap<PathBuf, Config>>> =
    std::sync::LazyLock::new(Default::default);

/// Locks the cache. A panic while the lock was held cannot leave the map
/// half-updated (every operation is one insert or lookup), so a poisoned lock
/// is used as-is.
fn lock_cache() -> std::sync::MutexGuard<'static, HashMap<PathBuf, Config>> {
    CACHE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Reads the config file (see [`load`] for the migration and reset rules),
/// bypassing the cache.
fn load_from_disk(config_dir: &Path) -> Result<Config, ProfileStoreError> {
    let path = config_dir.join(CONFIG_FILE_NAME);

    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return write_default_config(config_dir);
        }
        Err(e) => return Err(ProfileStoreError::Io(e)),
    };

    let Ok(table) = contents.parse::<toml::Table>() else {
        return reset_unreadable_config(config_dir, &contents);
    };

    // A legacy file also deserializes as the new shape (serde ignores the
    // unknown `presets` key and would silently drop the Custom region), so
    // the legacy marker is checked first.
    if table.contains_key("presets") {
        return match convert_legacy_config(table) {
            Some(config) => {
                // Keep the original preset-era file so the one-way migration is
                // reversible. An existing backup is never overwritten (a free numbered
                // name is used) and identical bytes are not backed up twice. If the copy
                // cannot be made, skip this write-back and retry on the next load; a
                // later save still writes the new shape over the original.
                if keep_pre_zone_backup(config_dir, &contents) {
                    // A failed write-back (read-only file, AV lock) must not
                    // make every caller fail; the config is valid in memory.
                    if let Err(e) = save(config_dir, &config) {
                        log!("GameFrame: could not write back migrated config: {e}");
                    }
                }
                Ok(config)
            }
            None => reset_unreadable_config(config_dir, &contents),
        };
    }

    // An older config may still carry per-game taskbar flags: fold them into
    // the global setting and write the new shape back.
    let mut table = table;
    let folded = fold_per_game_taskbar_flags(&mut table);

    match table.try_into::<Config>() {
        Ok(config) => {
            if folded && let Err(e) = save(config_dir, &config) {
                log!("GameFrame: could not write back migrated config: {e}");
            }
            Ok(config)
        }
        Err(_) => reset_unreadable_config(config_dir, &contents),
    }
}

/// Writes `contents` into a brand-new file at the first free name among
/// `<base>`, `<base>.1`, `<base>.2`, ... and returns the path used, or `None`
/// if an existing backup already holds exactly these bytes (nothing is written,
/// so retrying a load whose write-back keeps failing cannot pile up identical
/// copies). The destination is opened with `create_new`, so an existing backup
/// is never overwritten, even by an overlapping caller. Writing the bytes the
/// caller parsed (not re-reading the file) keeps the backup faithful to what
/// was converted.
fn write_free_backup(contents: &str, base: &Path) -> std::io::Result<Option<PathBuf>> {
    use std::io::Write;
    let mut n = 0u32;
    loop {
        let candidate = if n == 0 {
            base.to_path_buf()
        } else {
            let mut name = base.as_os_str().to_owned();
            name.push(format!(".{n}"));
            PathBuf::from(name)
        };
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut dest) => {
                if let Err(e) = dest.write_all(contents.as_bytes()) {
                    // Don't leave a partial copy that looks like a good backup.
                    drop(dest);
                    let _ = std::fs::remove_file(&candidate);
                    return Err(e);
                }
                return Ok(Some(candidate));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if std::fs::read(&candidate).is_ok_and(|b| b == contents.as_bytes()) {
                    return Ok(None);
                }
                n += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Backs up the preset-era `contents` to `config.toml.pre-zone.bak` (or the
/// first free `config.toml.pre-zone.bak.N`, so an earlier different original is
/// kept too). Returns whether a backup of these bytes is in place.
fn keep_pre_zone_backup(config_dir: &Path, contents: &str) -> bool {
    let base = config_dir.join(format!("{CONFIG_FILE_NAME}.pre-zone.bak"));
    match write_free_backup(contents, &base) {
        Ok(Some(backup)) => {
            log!(
                "GameFrame: migrated config to the Zone shape, original kept at {}",
                backup.display()
            );
            true
        }
        Ok(None) => true,
        Err(e) => {
            log!("GameFrame: could not back up the config before migrating: {e}");
            false
        }
    }
}

/// Backs up the `contents` of a config that is about to be replaced with
/// defaults to `config.toml.bak` (or the first free `config.toml.bak.N`), so an
/// earlier backup survives a second reset. Returns whether a backup of these
/// bytes is in place.
fn back_up_unreadable_config(config_dir: &Path, contents: &str) -> bool {
    let base = config_dir.join(format!("{CONFIG_FILE_NAME}.bak"));
    match write_free_backup(contents, &base) {
        Ok(Some(backup)) => {
            log!(
                "GameFrame: config could not be read; reset to defaults, original kept at {}",
                backup.display()
            );
            true
        }
        Ok(None) => true,
        Err(e) => {
            log!("GameFrame: config could not be read and backup failed: {e}");
            false
        }
    }
}

/// Backs the unreadable config up and replaces it with defaults. If the backup
/// fails the file is left alone and defaults are returned in memory only. That
/// is no lasting protection: a later save (Settings, overlay Zone Save or
/// Clear) writes over the original, and no copy of it was made.
fn reset_unreadable_config(config_dir: &Path, contents: &str) -> Result<Config, ProfileStoreError> {
    if back_up_unreadable_config(config_dir, contents) {
        write_default_config(config_dir)
    } else {
        Ok(Config::default())
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
    std::fs::rename(&temp_path, &final_path).map_err(ProfileStoreError::Io)?;

    lock_cache().insert(config_dir.to_path_buf(), config.clone());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config_file(config_dir: &Path, contents: &str) {
        std::fs::create_dir_all(config_dir).unwrap();
        std::fs::write(config_dir.join(CONFIG_FILE_NAME), contents).unwrap();
    }

    #[test]
    fn should_prompt_for_zone_when_no_zone_is_set() {
        assert!(should_prompt_for_zone(&Config::default()));
    }

    #[test]
    fn should_not_prompt_for_zone_when_a_zone_is_set() {
        let config = Config {
            zone: Some(Zone {
                x: 0,
                y: 0,
                width: 800,
                height: 600,
            }),
            ..Config::default()
        };
        assert!(!should_prompt_for_zone(&config));
    }

    #[test]
    fn load_on_empty_directory_creates_default_config_file_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");

        let config = load_from_disk(&config_dir).unwrap();

        assert_eq!(
            config,
            Config {
                zone: None,
                hotkeys: HotkeyBindings {
                    toggle_borderless: None,
                    toggle_taskbar_autohide: None,
                    toggle_game_cursor: None
                },
                tracked_games: Vec::new(),
                taskbar_hide_mode: TaskbarHideMode::FullyHidden,
                hide_taskbar_on_apply: false,
                cursor: CursorSettings::default(),
                update_mode: UpdateMode::Auto,
                log_ui_hangs: true,
            }
        );
        assert!(config_dir.join(CONFIG_FILE_NAME).exists());
    }

    #[test]
    fn a_new_shape_config_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");
        let written = Config {
            zone: Some(Zone {
                x: 1,
                y: 2,
                width: 300,
                height: 400,
            }),
            hotkeys: HotkeyBindings {
                toggle_borderless: Some("Ctrl+Alt+B".to_string()),
                toggle_taskbar_autohide: Some("Ctrl+Alt+T".to_string()),
                toggle_game_cursor: Some("Ctrl+Alt+M".to_string()),
            },
            tracked_games: vec![TrackedGame {
                exe_path: PathBuf::from("C:\\Games\\thing.exe"),
                display_name: "Thing".to_string(),
                auto_apply: false,
                game_cursor: false,
            }],
            taskbar_hide_mode: TaskbarHideMode::HoverReveal,
            hide_taskbar_on_apply: false,
            cursor: CursorSettings {
                enabled: true,
                size: 80,
                ..CursorSettings::default()
            },
            update_mode: UpdateMode::Ask,
            log_ui_hangs: false,
        };
        save(&config_dir, &written).unwrap();

        let loaded = load_from_disk(&config_dir).unwrap();

        assert_eq!(loaded, written);
    }

    #[test]
    fn log_ui_hangs_defaults_to_on_when_the_config_has_no_such_field() {
        let config: Config = toml::from_str("").unwrap();

        assert!(config.log_ui_hangs);
        assert!(Config::default().log_ui_hangs);
    }

    #[test]
    fn log_ui_hangs_round_trips_both_values_through_the_config_file() {
        for value in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let config_dir = dir.path().join("GameFrame");
            let written = Config {
                log_ui_hangs: value,
                ..Config::default()
            };
            save(&config_dir, &written).unwrap();

            let loaded = load_from_disk(&config_dir).unwrap();

            assert_eq!(loaded.log_ui_hangs, value);
        }
    }

    #[test]
    fn update_mode_defaults_to_auto_when_the_config_has_no_such_field() {
        let config: Config = toml::from_str("hide_taskbar_on_apply = true").unwrap();

        assert_eq!(config.update_mode, UpdateMode::Auto);
    }

    #[test]
    fn update_mode_round_trips_both_values_through_the_config_file() {
        for mode in [UpdateMode::Auto, UpdateMode::Ask] {
            let dir = tempfile::tempdir().unwrap();
            let config_dir = dir.path().join("GameFrame");
            let written = Config {
                update_mode: mode,
                ..Config::default()
            };
            save(&config_dir, &written).unwrap();

            let loaded = load_from_disk(&config_dir).unwrap();

            assert_eq!(loaded.update_mode, mode);
        }
    }

    #[test]
    fn update_mode_is_written_as_auto_or_ask() {
        for (mode, literal) in [
            (UpdateMode::Auto, "update_mode = \"auto\""),
            (UpdateMode::Ask, "update_mode = \"ask\""),
        ] {
            let config = Config {
                update_mode: mode,
                ..Config::default()
            };

            assert!(toml::to_string(&config).unwrap().contains(literal));
        }
    }

    #[test]
    fn load_of_a_corrupt_config_file_resets_to_defaults_instead_of_erroring() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");
        write_config_file(&config_dir, "this is not { valid toml : : :");

        let config = load_from_disk(&config_dir).unwrap();

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

        let config = load_from_disk(&config_dir).unwrap();

        assert_eq!(
            config.zone,
            Some(Zone {
                x: 10,
                y: 20,
                width: 200,
                height: 100
            })
        );
        assert_eq!(
            config.hotkeys.toggle_borderless,
            Some("Ctrl+Alt+B".to_string())
        );
        assert_eq!(
            config.hotkeys.toggle_taskbar_autohide,
            Some("Ctrl+Alt+T".to_string())
        );
        assert_eq!(config.taskbar_hide_mode, TaskbarHideMode::HoverReveal);
        assert_eq!(
            config.tracked_games,
            vec![
                TrackedGame {
                    exe_path: PathBuf::from("C:\\Games\\thing.exe"),
                    display_name: "Thing".to_string(),
                    auto_apply: false,
                    game_cursor: true,
                },
                TrackedGame {
                    exe_path: PathBuf::from("C:\\Games\\other.exe"),
                    display_name: "Other".to_string(),
                    auto_apply: true,
                    game_cursor: true,
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

        let config = load_from_disk(&config_dir).unwrap();

        assert_eq!(config.zone, None);
        assert_eq!(
            config.hotkeys.toggle_borderless,
            Some("Ctrl+Alt+B".to_string())
        );
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

        let migrated = load_from_disk(&config_dir).unwrap();

        let on_disk = std::fs::read_to_string(config_dir.join(CONFIG_FILE_NAME)).unwrap();
        assert!(
            !on_disk.contains("presets"),
            "the legacy [presets] table must be gone: {on_disk}"
        );
        assert!(
            !on_disk.contains("preset ="),
            "tracked games must no longer carry a preset: {on_disk}"
        );
        assert!(
            on_disk.contains("[zone]"),
            "the zone must be written: {on_disk}"
        );
        let second = load_from_disk(&config_dir).unwrap();
        assert_eq!(second, migrated);
    }

    /// Builds config TOML (legacy preset shape or Zone shape) with one tracked
    /// game per entry of `flags`, each carrying that per-game taskbar flag.
    fn config_with_game_flags(legacy: bool, flags: &[bool]) -> String {
        let mut s = String::new();
        if legacy {
            s.push_str("[presets]\nleft_depth_percent = 40\n\n");
        }
        s.push_str("[hotkeys]\n");
        for (i, flag) in flags.iter().enumerate() {
            s.push_str(&format!(
                "\n[[tracked_games]]\nexe_path = \"C:\\\\Games\\\\g{i}.exe\"\ndisplay_name = \"G{i}\"\nauto_apply = true\ntaskbar_autohide = {flag}\n"
            ));
            if legacy {
                s.push_str("preset = \"Left\"\n");
            }
        }
        s
    }

    fn assert_global_flag_derivation(legacy: bool, flags: &[bool], expected: bool) {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");
        write_config_file(&config_dir, &config_with_game_flags(legacy, flags));

        let config = load_from_disk(&config_dir).unwrap();

        assert_eq!(
            config.hide_taskbar_on_apply, expected,
            "legacy={legacy} flags={flags:?}"
        );
        assert_eq!(config.tracked_games.len(), flags.len());
        let on_disk = std::fs::read_to_string(config_dir.join(CONFIG_FILE_NAME)).unwrap();
        assert!(
            !on_disk
                .lines()
                .any(|l| l.trim_start().starts_with("taskbar_autohide =")),
            "per-game flags must be gone: {on_disk}"
        );
        assert!(
            on_disk.contains(&format!("hide_taskbar_on_apply = {expected}")),
            "{on_disk}"
        );
        assert_eq!(
            load_from_disk(&config_dir).unwrap(),
            config,
            "the written-back file must read back identically"
        );
    }

    #[test]
    fn global_taskbar_flag_is_on_when_any_game_had_it_on() {
        assert_global_flag_derivation(true, &[false, true], true);
        assert_global_flag_derivation(false, &[false, true], true);
    }

    #[test]
    fn global_taskbar_flag_is_off_when_every_game_had_it_off() {
        assert_global_flag_derivation(true, &[false, false], false);
        assert_global_flag_derivation(false, &[false, false], false);
    }

    #[test]
    fn global_taskbar_flag_is_off_when_there_were_no_games() {
        assert_global_flag_derivation(true, &[], false);
    }

    #[test]
    fn hide_taskbar_on_apply_is_off_by_default() {
        assert!(!Config::default().hide_taskbar_on_apply);
    }

    #[test]
    fn a_config_file_without_hide_taskbar_on_apply_loads_it_as_off() {
        let config: Config = toml::from_str("").unwrap();

        assert!(!config.hide_taskbar_on_apply);
    }

    #[test]
    fn an_explicit_hide_taskbar_on_apply_true_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");
        write_config_file(&config_dir, "hide_taskbar_on_apply = true\n");

        let loaded = load_from_disk(&config_dir).unwrap();
        assert!(loaded.hide_taskbar_on_apply);

        save(&config_dir, &loaded).unwrap();
        assert!(load_from_disk(&config_dir).unwrap().hide_taskbar_on_apply);
    }

    #[test]
    fn an_existing_global_flag_is_not_overridden_by_stale_per_game_flags() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");
        write_config_file(
            &config_dir,
            &format!(
                "hide_taskbar_on_apply = false\n{}",
                config_with_game_flags(false, &[true])
            ),
        );

        assert!(!load_from_disk(&config_dir).unwrap().hide_taskbar_on_apply);
    }

    #[test]
    fn migrating_a_legacy_config_keeps_a_pre_zone_backup_and_never_overwrites_it() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");
        write_config_file(&config_dir, LEGACY_WITH_CUSTOM);
        let backup = config_dir.join("config.toml.pre-zone.bak");

        load_from_disk(&config_dir).unwrap();

        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            LEGACY_WITH_CUSTOM
        );

        // A second preset-era file later (e.g. after a downgrade) must not
        // clobber the first backup, and its own original must be kept too.
        let second = config_with_game_flags(true, &[true]);
        write_config_file(&config_dir, &second);
        load_from_disk(&config_dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            LEGACY_WITH_CUSTOM
        );
        assert_eq!(
            std::fs::read_to_string(config_dir.join("config.toml.pre-zone.bak.1")).unwrap(),
            second
        );
    }

    #[test]
    fn a_second_unreadable_config_does_not_destroy_the_earlier_backup() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");
        write_config_file(&config_dir, "first { broken");
        load_from_disk(&config_dir).unwrap();
        write_config_file(&config_dir, "second { broken");

        load_from_disk(&config_dir).unwrap();

        assert_eq!(
            std::fs::read_to_string(config_dir.join("config.toml.bak")).unwrap(),
            "first { broken"
        );
        assert_eq!(
            std::fs::read_to_string(config_dir.join("config.toml.bak.1")).unwrap(),
            "second { broken"
        );
    }

    /// Makes every `save` into `config_dir` fail: its temp file's name is
    /// taken by a directory.
    fn block_saves(config_dir: &Path) {
        std::fs::create_dir(config_dir.join(format!("{CONFIG_FILE_NAME}.tmp"))).unwrap();
    }

    fn backup_files(config_dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(config_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".bak"))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn repeated_loads_of_an_unsavable_legacy_config_keep_exactly_one_backup() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");
        write_config_file(&config_dir, LEGACY_WITH_CUSTOM);
        block_saves(&config_dir);

        for _ in 0..3 {
            load_from_disk(&config_dir).unwrap();
        }

        assert_eq!(backup_files(&config_dir), ["config.toml.pre-zone.bak"]);
    }

    #[test]
    fn repeated_loads_of_an_unreadable_config_that_cannot_be_reset_keep_exactly_one_backup() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");
        write_config_file(&config_dir, "broken { toml");
        block_saves(&config_dir);

        for _ in 0..3 {
            let _ = load_from_disk(&config_dir);
        }

        assert_eq!(backup_files(&config_dir), ["config.toml.bak"]);
    }

    #[test]
    fn load_of_an_unconvertible_old_shape_config_resets_to_defaults() {
        // An old shape (`presets` as an array of tagged variants) is
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

        let config = load_from_disk(&config_dir).unwrap();

        assert_eq!(config, Config::default());
        assert_eq!(load_from_disk(&config_dir).unwrap(), Config::default());
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

        let config = load_from_disk(&config_dir).unwrap();

        assert_eq!(config.zone, None);
        assert_eq!(config.tracked_games, Vec::new());
        assert_eq!(config.taskbar_hide_mode, TaskbarHideMode::FullyHidden);
        assert_eq!(
            config.hotkeys.toggle_borderless,
            Some("Ctrl+Alt+B".to_string())
        );
    }

    #[test]
    fn a_config_without_a_hotkeys_table_keeps_its_zone_and_tracked_games() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");
        write_config_file(
            &config_dir,
            r#"
            [zone]
            x = 10
            y = 20
            width = 800
            height = 600

            [[tracked_games]]
            exe_path = "C:\\Games\\thing.exe"
            display_name = "Thing"
            auto_apply = false
            "#,
        );

        let config = load(&config_dir).unwrap();

        assert_eq!(config.hotkeys, HotkeyBindings::default());
        assert_eq!(
            config.zone,
            Some(Zone {
                x: 10,
                y: 20,
                width: 800,
                height: 600
            })
        );
        assert_eq!(config.tracked_games.len(), 1);
        assert_eq!(config.tracked_games[0].display_name, "Thing");
        assert!(backup_files(&config_dir).is_empty());
    }

    #[test]
    fn later_loads_are_served_from_memory_without_reading_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");
        let first = load(&config_dir).unwrap();

        std::fs::remove_file(config_dir.join(CONFIG_FILE_NAME)).unwrap();

        assert_eq!(load(&config_dir).unwrap(), first);
        assert!(!config_dir.join(CONFIG_FILE_NAME).exists());
    }

    #[test]
    fn load_serves_a_migrated_config_from_memory() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");
        write_config_file(&config_dir, LEGACY_WITH_CUSTOM);
        let migrated = load(&config_dir).unwrap();
        assert!(migrated.zone.is_some());

        write_config_file(&config_dir, "not { toml");

        assert_eq!(load(&config_dir).unwrap(), migrated);
        assert_eq!(backup_files(&config_dir), ["config.toml.pre-zone.bak"]);
    }

    #[test]
    fn load_serves_a_reset_default_config_from_memory() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");
        write_config_file(&config_dir, "first { broken");
        assert_eq!(load(&config_dir).unwrap(), Config::default());

        write_config_file(&config_dir, "second { broken");

        assert_eq!(load(&config_dir).unwrap(), Config::default());
        assert_eq!(backup_files(&config_dir), ["config.toml.bak"]);
    }

    #[test]
    fn save_refreshes_what_load_returns() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");
        let mut config = load(&config_dir).unwrap();
        config.hide_taskbar_on_apply = false;
        config.hotkeys.toggle_borderless = Some("Ctrl+Alt+B".to_string());

        save(&config_dir, &config).unwrap();

        assert_eq!(load(&config_dir).unwrap(), config);
    }

    #[test]
    fn a_failed_save_leaves_the_cached_config_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");
        let original = load(&config_dir).unwrap();
        block_saves(&config_dir);
        let mut changed = original.clone();
        changed.hide_taskbar_on_apply = !original.hide_taskbar_on_apply;

        assert!(save(&config_dir, &changed).is_err());

        assert_eq!(load(&config_dir).unwrap(), original);
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
        assert_eq!(
            reloaded.hotkeys.toggle_borderless,
            Some("Ctrl+Alt+B".to_string())
        );
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
    fn tracked_game_new_default_has_auto_apply_on() {
        let game =
            TrackedGame::new_default(PathBuf::from("C:\\Games\\thing.exe"), "Thing".to_string());

        assert_eq!(game.exe_path, PathBuf::from("C:\\Games\\thing.exe"));
        assert_eq!(game.display_name, "Thing");
        assert!(game.auto_apply);
    }

    #[test]
    fn upsert_tracked_game_appends_a_new_default_entry_when_no_existing_match() {
        let mut games: Vec<TrackedGame> = Vec::new();

        let index = upsert_tracked_game(
            &mut games,
            PathBuf::from("C:\\Games\\thing.exe"),
            "Thing".to_string(),
        );

        assert_eq!(index, 0);
        assert_eq!(games.len(), 1);
        assert_eq!(games[0].display_name, "Thing");
        assert!(games[0].auto_apply);
    }

    #[test]
    fn upsert_tracked_game_reuses_the_existing_entry_for_a_matching_executable_path() {
        let mut games = vec![TrackedGame::new_default(
            PathBuf::from("C:\\Games\\thing.exe"),
            "Thing".to_string(),
        )];
        games[0].auto_apply = false;

        let index = upsert_tracked_game(
            &mut games,
            PathBuf::from("C:\\Games\\thing.exe"),
            "Thing (different window title)".to_string(),
        );

        assert_eq!(index, 0);
        assert_eq!(
            games.len(),
            1,
            "picking an already-tracked game again must not create a duplicate entry"
        );
        // Display name is captured only at add-time.
        assert_eq!(games[0].display_name, "Thing");
        // Existing settings are preserved untouched, not reset to defaults.
        assert!(!games[0].auto_apply);
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
        )];

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

        assert_eq!(
            io_err.to_string(),
            "config I/O error: config file not found"
        );
        assert!(
            serialize_err
                .to_string()
                .starts_with("failed to serialize config: ")
        );
    }
}
