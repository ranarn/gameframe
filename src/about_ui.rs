//! The Settings window's About tab: version, license, source link and the
//! voluntary donation link.

/// The license shown in the About tab, from `Cargo.toml` (single source).
const LICENSE: &str = env!("CARGO_PKG_LICENSE");

/// The repository's funding config, embedded so the donation link has a
/// single source of truth. This reaches outside `src/`, so it assumes a
/// repo-only build: `cargo package`/`publish` without `.github` would break it.
const FUNDING_YML: &str = include_str!("../.github/FUNDING.yml");

/// The first `custom:` URL in a `FUNDING.yml` body. This is an ad-hoc scraper
/// for the single-line forms only: a bare or quoted value (`custom: https://...`,
/// `custom: "https://..."`) or a flow list (`custom: ["https://...", ...]`),
/// of which the first entry counts. A quoted value may contain a `,`; so may a
/// bare value outside a list. A multi-line (block) list yields `None`. The test
/// `repo_funding_config_yields_a_donation_url` guards the repo file against
/// drifting into an unsupported form.
fn funding_custom_url(funding_yml: &str) -> Option<String> {
    let rest = funding_yml
        .lines()
        .find_map(|line| line.trim().strip_prefix("custom:"))?
        .trim();
    let (value, in_list) = match rest.strip_prefix('[') {
        Some(items) => (items.trim_start(), true),
        None => (rest, false),
    };
    let url: String = match value.chars().next()? {
        quote @ ('"' | '\'') => value[1..].chars().take_while(|&c| c != quote).collect(),
        _ => value
            .chars()
            .take_while(|&c| !c.is_whitespace() && !(in_list && matches!(c, ',' | ']')))
            .collect(),
    };
    url.starts_with("https://").then_some(url)
}

fn donation_url() -> Option<String> {
    funding_custom_url(FUNDING_YML)
}

/// Renders the About tab: version, license, source link and the
/// voluntary donation link.
pub(crate) fn about_section(ui: &mut egui::Ui) {
    ui.heading("GameFrame");
    ui.label(crate::version::current_label());
    ui.label(format!("License: {LICENSE}"));
    ui.hyperlink_to("Source code", env!("CARGO_PKG_REPOSITORY"));
    ui.separator();
    ui.heading("Support");
    ui.label("GameFrame is free. Donating is entirely optional.");
    match donation_url() {
        Some(url) => {
            ui.hyperlink_to("Donate via PayPal (optional)", url);
        }
        None => {
            ui.label("No donation link is configured in this build.");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn funding_custom_url_is_read_from_the_yaml() {
        assert_eq!(
            funding_custom_url("custom: [\"https://example.com/d?x=1\"]\n").as_deref(),
            Some("https://example.com/d?x=1")
        );
        assert_eq!(funding_custom_url("github: someone\n"), None);
    }

    #[test]
    fn funding_custom_url_keeps_a_comma_inside_a_quoted_url() {
        assert_eq!(
            funding_custom_url("custom: [\"https://example.com/d?a=1,2\", \"https://other\"]\n")
                .as_deref(),
            Some("https://example.com/d?a=1,2")
        );
        assert_eq!(
            funding_custom_url("custom: 'https://example.com/d?a=1,2'\n").as_deref(),
            Some("https://example.com/d?a=1,2")
        );
        assert_eq!(
            funding_custom_url("custom: https://example.com/d?a=1,2\n").as_deref(),
            Some("https://example.com/d?a=1,2")
        );
    }

    #[test]
    fn funding_custom_url_takes_the_first_entry_of_an_unquoted_list() {
        assert_eq!(
            funding_custom_url("custom: [https://one.example, https://two.example]\n").as_deref(),
            Some("https://one.example")
        );
    }

    #[test]
    fn funding_custom_url_yields_none_for_an_empty_or_block_list_or_a_non_https_value() {
        assert_eq!(funding_custom_url("custom: []\n"), None);
        assert_eq!(
            funding_custom_url("custom:\n  - https://one.example\n"),
            None
        );
        assert_eq!(funding_custom_url("custom: http://plain.example\n"), None);
    }

    #[test]
    fn repo_funding_config_yields_a_donation_url() {
        assert!(
            donation_url()
                .unwrap()
                .starts_with("https://www.paypal.com/")
        );
    }
}
