//! The one place the running version is turned into user-facing text, so the
//! About tab, the Settings footer and the tray tooltip can't disagree.

/// The `0.0.0` placeholder that unreleased builds carry.
const DEV_VERSION: &str = "0.0.0";

/// The version line for the About tab and the Settings footer; the `0.0.0`
/// placeholder that unreleased builds carry is shown as a development build.
pub fn version_label(version: &str) -> String {
    if version == DEV_VERSION {
        "Version: development build".to_string()
    } else {
        format!("Version: {version}")
    }
}

/// The tray icon's hover text for `version`, e.g. "GameFrame 0.3.0".
pub fn tooltip_text(version: &str) -> String {
    if version == DEV_VERSION {
        "GameFrame (development build)".to_string()
    } else {
        format!("GameFrame {version}")
    }
}

/// [`version_label`] for the running build.
pub fn current_label() -> String {
    version_label(env!("CARGO_PKG_VERSION"))
}

/// [`tooltip_text`] for the running build: the tray icon's hover text when no
/// update is downloading.
pub fn current_tooltip() -> String {
    tooltip_text(env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_version_is_shown_as_development_build() {
        assert_eq!(version_label("0.0.0"), "Version: development build");
        assert_eq!(version_label("1.2.3"), "Version: 1.2.3");
    }

    #[test]
    fn tooltip_includes_the_version() {
        assert_eq!(tooltip_text("0.3.0"), "GameFrame 0.3.0");
        assert_eq!(tooltip_text("0.0.0"), "GameFrame (development build)");
    }
}
