//! Loads and saves GameFrame's config (presets + hotkey bindings) as TOML,
//! under a config directory injected by the caller. This module never
//! resolves `%APPDATA%` itself — the caller passes in the directory to use.

use crate::region_calc::Preset;
use std::path::Path;

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
}

impl Default for Presets {
    fn default() -> Self {
        Presets {
            left_depth_percent: DEFAULT_DEPTH_PERCENT,
            right_depth_percent: DEFAULT_DEPTH_PERCENT,
            custom: None,
        }
    }
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
}

/// GameFrame's persisted config: the preset set plus the two global hotkey
/// bindings.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Config {
    pub presets: Presets,
    pub hotkeys: HotkeyBindings,
}

impl Default for Config {
    /// The starter config written on first run: default-depth Left/Right, no
    /// Custom, and both hotkeys unbound.
    fn default() -> Self {
        Config { presets: Presets::default(), hotkeys: HotkeyBindings::default() }
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
                },
                hotkeys: HotkeyBindings { toggle_borderless: None, toggle_taskbar_autohide: None },
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
            },
            hotkeys: HotkeyBindings {
                toggle_borderless: Some("Ctrl+Alt+B".to_string()),
                toggle_taskbar_autohide: Some("Ctrl+Alt+T".to_string()),
            },
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
