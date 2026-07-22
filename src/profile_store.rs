//! Loads and saves GameFrame's config (presets + hotkey bindings) as TOML,
//! under a config directory injected by the caller. This module never
//! resolves `%APPDATA%` itself — the caller passes in the directory to use.
//!
//! `#[allow(dead_code)]`: this module has no consumers yet (no settings-UI or
//! hotkey-wiring slice has landed) — see issue #3. Remove once it's wired up.
#![allow(dead_code)]

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

/// GameFrame's persisted config: the preset list plus the two global hotkey
/// bindings.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Config {
    pub presets: Vec<Preset>,
    pub hotkeys: HotkeyBindings,
}

impl Default for Config {
    /// The starter config written on first run: a small preset list and both
    /// hotkeys unbound.
    fn default() -> Self {
        Config {
            presets: vec![Preset::LeftHalf, Preset::RightHalf, Preset::FullScreen],
            hotkeys: HotkeyBindings::default(),
        }
    }
}

/// Reasons loading or saving the config can fail.
#[derive(Debug)]
pub enum ProfileStoreError {
    /// A filesystem operation (read, write, or directory creation) failed.
    Io(std::io::Error),
    /// The config file exists but its contents are not valid config TOML.
    Malformed(toml::de::Error),
    /// The in-memory config could not be serialized to TOML (should not
    /// normally happen for a valid `Config`).
    Serialize(toml::ser::Error),
}

impl std::fmt::Display for ProfileStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProfileStoreError::Io(e) => write!(f, "config I/O error: {e}"),
            ProfileStoreError::Malformed(e) => write!(f, "config file is malformed: {e}"),
            ProfileStoreError::Serialize(e) => write!(f, "failed to serialize config: {e}"),
        }
    }
}

impl std::error::Error for ProfileStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ProfileStoreError::Io(e) => Some(e),
            ProfileStoreError::Malformed(e) => Some(e),
            ProfileStoreError::Serialize(e) => Some(e),
        }
    }
}

pub fn load(config_dir: &Path) -> Result<Config, ProfileStoreError> {
    let path = config_dir.join(CONFIG_FILE_NAME);

    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let default_config = Config::default();
            save(config_dir, &default_config)?;
            return Ok(default_config);
        }
        Err(e) => return Err(ProfileStoreError::Io(e)),
    };

    toml::from_str(&contents).map_err(ProfileStoreError::Malformed)
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
                presets: vec![Preset::LeftHalf, Preset::RightHalf, Preset::FullScreen],
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
            presets: vec![Preset::Custom { x: 1, y: 2, width: 300, height: 400 }],
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
    fn load_of_a_corrupt_config_file_returns_a_malformed_error_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join(CONFIG_FILE_NAME), "this is not { valid toml : : :").unwrap();

        let result = load(&config_dir);

        assert!(matches!(result, Err(ProfileStoreError::Malformed(_))));
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
    fn profile_store_error_implements_display_and_std_error() {
        fn assert_is_std_error<E: std::error::Error>(_: &E) {}

        let io_err = ProfileStoreError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "config file not found",
        ));
        let malformed_err = match toml::from_str::<Config>("this is not { valid toml : : :") {
            Err(e) => ProfileStoreError::Malformed(e),
            Ok(_) => panic!("expected malformed toml to fail parsing"),
        };
        let serialize_err = match toml::to_string(&5i32) {
            Err(e) => ProfileStoreError::Serialize(e),
            Ok(_) => panic!("expected serializing a bare integer to fail"),
        };

        assert_is_std_error(&io_err);
        assert_is_std_error(&malformed_err);
        assert_is_std_error(&serialize_err);

        assert_eq!(io_err.to_string(), "config I/O error: config file not found");
        assert!(malformed_err.to_string().starts_with("config file is malformed: "));
        assert!(serialize_err.to_string().starts_with("failed to serialize config: "));
    }
}
