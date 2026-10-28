//! Global hotkey registration/dispatch, built on the `global-hotkey` crate.
//!
//! Split the same way as [`crate::win32`]:
//! - Parsing a stored hotkey-binding string and resolving which action a
//!   registered hotkey's id maps back to is pure logic with no OS dependency,
//!   fully unit tested below.
//! - Actually registering a hotkey with the OS
//!   ([`global_hotkey::GlobalHotKeyManager`]) is OS-facing: per the crate's
//!   own docs it requires a live Win32 event loop already pumping on this
//!   thread, which a headless/CI sandbox can't provide -- so [`Hotkeys::register`]
//!   itself is NOT exercised by this crate's automated tests.

use crate::profile_store::HotkeyBindings;
use global_hotkey::GlobalHotKeyManager;
use global_hotkey::hotkey::HotKey;

/// Actions GameFrame can bind a global hotkey to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)] // every action is a toggle
pub enum HotkeyAction {
    ToggleBorderless,
    /// Toggles the taskbar hidden/visible, using whichever technique
    /// [`crate::profile_store::TaskbarHideMode`] currently selects -- see
    /// [`crate::taskbar::toggle_taskbar_hide`]. Fully independent of
    /// [`HotkeyAction::ToggleBorderless`]'s state; either can be pressed
    /// without the other having been used first.
    ToggleTaskbarAutohide,
    /// Turns the game cursor on/off (see [`crate::cursor_style`]),
    /// persisting the choice.
    ToggleGameCursor,
}

impl HotkeyAction {
    fn label(self) -> &'static str {
        match self {
            HotkeyAction::ToggleBorderless => "toggle borderless",
            HotkeyAction::ToggleTaskbarAutohide => "toggle taskbar auto-hide",
            HotkeyAction::ToggleGameCursor => "toggle game cursor",
        }
    }
}

/// A hotkey binding resolved from its stored string into the parsed
/// [`HotKey`] the `global-hotkey` crate needs, paired with the action it
/// should trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedBinding {
    pub action: HotkeyAction,
    pub hotkey: HotKey,
}

/// Returns the [`HotkeyAction`] a registered hotkey id maps to, or `None` if
/// `id` doesn't match any of `bindings` -- e.g. because nothing is bound, or
/// the id belongs to some other hotkey entirely.
pub fn action_for(bindings: &[ResolvedBinding], id: u32) -> Option<HotkeyAction> {
    bindings
        .iter()
        .find(|b| b.hotkey.id() == id)
        .map(|b| b.action)
}

/// What happened to one non-empty binding when registering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingOutcome {
    Registered,
    /// Human-readable reason, shown in Settings next to the binding.
    Failed(String),
}

/// The outcome for one non-empty binding, with the binding string it was
/// for (so Settings can tell whether the user has since changed it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingReport {
    pub action: HotkeyAction,
    pub binding: String,
    pub outcome: BindingOutcome,
}

/// The notice for bindings that fail in `now` but did not fail the same way in
/// `previous` (the registration before Settings was opened): the ones the user
/// just saved. A failure that was already there is not repeated on every visit
/// to Settings. `None` when there is nothing new.
pub(crate) fn new_failure_notice(
    previous: &[BindingReport],
    now: &[BindingReport],
) -> Option<String> {
    let lines: Vec<String> = now
        .iter()
        .filter_map(|report| {
            let BindingOutcome::Failed(reason) = &report.outcome else {
                return None;
            };
            let known = previous.iter().any(|p| {
                p.action == report.action
                    && p.binding == report.binding
                    && matches!(p.outcome, BindingOutcome::Failed(_))
            });
            (!known).then(|| format!("{} ({}): {reason}", report.binding, report.action.label()))
        })
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(format!(
        "Couldn't register {}.\nChange it in Settings.",
        lines.join("; ")
    ))
}

/// Human-readable reason an OS registration failed.
fn registration_failure_reason(error: &global_hotkey::Error) -> String {
    match error {
        global_hotkey::Error::AlreadyRegistered(_) => {
            "already in use by another program".to_string()
        }
        other => format!("the OS refused the hotkey: {other}"),
    }
}

/// Registers every non-empty binding on its own through `register`. Binding
/// strings (free text from Settings) are parsed with `HotKey`'s `FromStr`,
/// which accepts the `"Ctrl+Alt+B"` syntax Settings stores. A parse
/// or registration failure of one binding is reported for that binding only
/// and never blocks the others. Returns the bindings that registered (for
/// [`action_for`]) and one report per non-empty binding. Empty (unbound)
/// bindings are skipped, not failures.
///
/// `register` abstracts the OS call so this logic is testable.
pub(crate) fn register_each(
    bindings: &HotkeyBindings,
    mut register: impl FnMut(HotKey) -> Result<(), String>,
) -> (Vec<ResolvedBinding>, Vec<BindingReport>) {
    let candidates = [
        (HotkeyAction::ToggleBorderless, &bindings.toggle_borderless),
        (
            HotkeyAction::ToggleTaskbarAutohide,
            &bindings.toggle_taskbar_autohide,
        ),
        (HotkeyAction::ToggleGameCursor, &bindings.toggle_game_cursor),
    ];
    let mut registered: Vec<ResolvedBinding> = Vec::new();
    let mut reports = Vec::new();
    for (action, raw) in candidates {
        let Some(raw) = raw.as_ref().filter(|r| !r.trim().is_empty()) else {
            continue;
        };
        let outcome = match raw.parse::<HotKey>() {
            Err(e) => BindingOutcome::Failed(format!("not a valid hotkey: {e}")),
            // Same combination already held by GameFrame's own earlier binding
            // (hand-edited config): say so rather than blaming another program.
            Ok(hotkey) => match registered.iter().find(|b| b.hotkey == hotkey) {
                Some(other) => BindingOutcome::Failed(format!(
                    "already bound to another GameFrame action ({})",
                    other.action.label()
                )),
                None => match register(hotkey) {
                    Ok(()) => {
                        registered.push(ResolvedBinding { action, hotkey });
                        BindingOutcome::Registered
                    }
                    Err(reason) => BindingOutcome::Failed(reason),
                },
            },
        };
        reports.push(BindingReport {
            action,
            binding: raw.clone(),
            outcome,
        });
    }
    (registered, reports)
}

/// Owns the live global-hotkey registration for GameFrame's bound actions.
///
/// OS-facing: [`Hotkeys::register`] creates a real `GlobalHotKeyManager`,
/// which per the `global-hotkey` crate's own docs requires a live Win32
/// event loop pumping on this thread (see [`crate::app::run_event_loop`]) --
/// a headless/CI sandbox has no such loop, so this is NOT exercised by this
/// crate's automated tests. The per-binding logic it delegates to
/// ([`register_each`]) and the id -> action lookup ([`action_for`]) are
/// exercised in isolation from the OS call.
pub struct Hotkeys {
    // Kept alive for its `Drop` impl, which unregisters every hotkey and
    // tears down the manager's hidden window -- never read directly.
    _manager: GlobalHotKeyManager,
    bindings: Vec<ResolvedBinding>,
}

impl Hotkeys {
    /// Registers every bound hotkey in `bindings` with the OS, each on its
    /// own (see [`register_each`]). Only the bindings that registered
    /// resolve in [`Self::action_for`]; the report says what happened to each.
    /// If the OS hotkey manager can't be created, no `Hotkeys` is returned and
    /// every non-empty binding is reported as failed with that reason, so
    /// Settings still shows it.
    pub fn register(bindings: &HotkeyBindings) -> (Option<Self>, Vec<BindingReport>) {
        match GlobalHotKeyManager::new() {
            Ok(manager) => {
                let (registered, reports) = register_each(bindings, |hotkey| {
                    manager
                        .register(hotkey)
                        .map_err(|e| registration_failure_reason(&e))
                });
                (
                    Some(Self {
                        _manager: manager,
                        bindings: registered,
                    }),
                    reports,
                )
            }
            Err(e) => {
                let reason = format!("the hotkey manager could not be created: {e}");
                let (_, reports) = register_each(bindings, |_| Err(reason.clone()));
                (None, reports)
            }
        }
    }

    /// The action a received [`global_hotkey::GlobalHotKeyEvent`]'s id maps
    /// to, or `None` if it doesn't match any of GameFrame's registered
    /// hotkeys.
    pub fn action_for(&self, id: u32) -> Option<HotkeyAction> {
        action_for(&self.bindings, id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_each_maps_each_binding_to_its_own_action() {
        for (bindings, action, key) in [
            (
                HotkeyBindings {
                    toggle_borderless: Some("Ctrl+Alt+B".to_string()),
                    ..HotkeyBindings::default()
                },
                HotkeyAction::ToggleBorderless,
                "Ctrl+Alt+B",
            ),
            (
                HotkeyBindings {
                    toggle_taskbar_autohide: Some("Ctrl+Alt+T".to_string()),
                    ..HotkeyBindings::default()
                },
                HotkeyAction::ToggleTaskbarAutohide,
                "Ctrl+Alt+T",
            ),
            (
                HotkeyBindings {
                    toggle_game_cursor: Some("Ctrl+Alt+M".to_string()),
                    ..HotkeyBindings::default()
                },
                HotkeyAction::ToggleGameCursor,
                "Ctrl+Alt+M",
            ),
        ] {
            let (registered, _) = register_each(&bindings, |_| Ok(()));

            assert_eq!(registered.len(), 1);
            assert_eq!(registered[0].action, action);
            assert_eq!(registered[0].hotkey, key.parse::<HotKey>().unwrap());
        }
    }

    #[test]
    fn register_each_with_nothing_bound_registers_nothing() {
        let (registered, reports) = register_each(&HotkeyBindings::default(), |_| Ok(()));

        assert!(registered.is_empty());
        assert!(reports.is_empty());
    }

    #[test]
    fn action_for_finds_the_action_bound_to_a_matching_hotkey_id() {
        let hotkey: HotKey = "Ctrl+Alt+B".parse().unwrap();
        let bindings = vec![ResolvedBinding {
            action: HotkeyAction::ToggleBorderless,
            hotkey,
        }];

        assert_eq!(
            action_for(&bindings, hotkey.id()),
            Some(HotkeyAction::ToggleBorderless)
        );
    }

    #[test]
    fn action_for_returns_none_when_no_binding_matches_the_id() {
        let hotkey: HotKey = "Ctrl+Alt+B".parse().unwrap();
        let bindings = vec![ResolvedBinding {
            action: HotkeyAction::ToggleBorderless,
            hotkey,
        }];

        assert_eq!(action_for(&bindings, 999_999), None);
    }

    #[test]
    fn action_for_dispatches_to_the_correct_action_when_both_hotkeys_are_bound() {
        // Independence check: with both bound, pressing one hotkey's id must
        // resolve to its own action only, never the other's, and vice versa.
        let borderless_hotkey: HotKey = "Ctrl+Alt+B".parse().unwrap();
        let taskbar_hotkey: HotKey = "Ctrl+Alt+T".parse().unwrap();
        let bindings = vec![
            ResolvedBinding {
                action: HotkeyAction::ToggleBorderless,
                hotkey: borderless_hotkey,
            },
            ResolvedBinding {
                action: HotkeyAction::ToggleTaskbarAutohide,
                hotkey: taskbar_hotkey,
            },
        ];

        assert_eq!(
            action_for(&bindings, borderless_hotkey.id()),
            Some(HotkeyAction::ToggleBorderless)
        );
        assert_eq!(
            action_for(&bindings, taskbar_hotkey.id()),
            Some(HotkeyAction::ToggleTaskbarAutohide)
        );
    }

    #[test]
    fn action_for_on_an_empty_binding_list_returns_none_without_panicking() {
        // The fresh-install / nothing-bound case: pressing any key must not
        // crash and must trigger no action.
        assert_eq!(action_for(&[], 42), None);
    }

    fn all_three() -> HotkeyBindings {
        HotkeyBindings {
            toggle_borderless: Some("Ctrl+Alt+B".to_string()),
            toggle_taskbar_autohide: Some("Ctrl+Alt+T".to_string()),
            toggle_game_cursor: Some("Ctrl+Alt+M".to_string()),
        }
    }

    fn outcome_of(reports: &[BindingReport], action: HotkeyAction) -> &BindingOutcome {
        &reports.iter().find(|r| r.action == action).unwrap().outcome
    }

    #[test]
    fn register_each_registers_every_binding_when_all_succeed() {
        let (registered, reports) = register_each(&all_three(), |_| Ok(()));

        assert_eq!(registered.len(), 3);
        assert_eq!(reports.len(), 3);
        assert!(
            reports
                .iter()
                .all(|r| r.outcome == BindingOutcome::Registered)
        );
    }

    #[test]
    fn register_each_one_failure_leaves_the_others_registered_and_dispatchable() {
        let taken: HotKey = "Ctrl+Alt+T".parse().unwrap();
        let (registered, reports) = register_each(&all_three(), |hotkey| {
            if hotkey == taken {
                Err("already in use by another program".to_string())
            } else {
                Ok(())
            }
        });

        assert_eq!(
            outcome_of(&reports, HotkeyAction::ToggleTaskbarAutohide),
            &BindingOutcome::Failed("already in use by another program".to_string())
        );
        assert_eq!(
            outcome_of(&reports, HotkeyAction::ToggleBorderless),
            &BindingOutcome::Registered
        );
        assert_eq!(
            outcome_of(&reports, HotkeyAction::ToggleGameCursor),
            &BindingOutcome::Registered
        );
        assert_eq!(registered.len(), 2);
        assert_eq!(action_for(&registered, taken.id()), None);
        let borderless: HotKey = "Ctrl+Alt+B".parse().unwrap();
        assert_eq!(
            action_for(&registered, borderless.id()),
            Some(HotkeyAction::ToggleBorderless)
        );
    }

    #[test]
    fn register_each_reports_every_binding_failed_when_all_fail() {
        let (registered, reports) = register_each(&all_three(), |_| Err("nope".to_string()));

        assert!(registered.is_empty());
        assert_eq!(reports.len(), 3);
        assert!(
            reports
                .iter()
                .all(|r| r.outcome == BindingOutcome::Failed("nope".to_string()))
        );
    }

    #[test]
    fn register_each_unparseable_binding_fails_alone_without_calling_the_os() {
        let bindings = HotkeyBindings {
            toggle_borderless: Some("not a real hotkey format!!".to_string()),
            toggle_taskbar_autohide: Some("Ctrl+Alt+T".to_string()),
            toggle_game_cursor: None,
        };
        let mut os_calls = 0;

        let (registered, reports) = register_each(&bindings, |_| {
            os_calls += 1;
            Ok(())
        });

        assert_eq!(os_calls, 1);
        assert!(matches!(
            outcome_of(&reports, HotkeyAction::ToggleBorderless),
            BindingOutcome::Failed(reason) if reason.starts_with("not a valid hotkey")
        ));
        assert_eq!(
            outcome_of(&reports, HotkeyAction::ToggleTaskbarAutohide),
            &BindingOutcome::Registered
        );
        assert_eq!(registered.len(), 1);
    }

    #[test]
    fn register_each_same_combo_on_two_actions_blames_the_earlier_gameframe_binding() {
        let bindings = HotkeyBindings {
            toggle_borderless: Some("Ctrl+Alt+B".to_string()),
            toggle_game_cursor: Some("Ctrl+Alt+B".to_string()),
            ..HotkeyBindings::default()
        };
        let mut os_calls = 0;

        let (registered, reports) = register_each(&bindings, |_| {
            os_calls += 1;
            Ok(())
        });

        assert_eq!(os_calls, 1);
        assert_eq!(registered.len(), 1);
        assert!(matches!(
            outcome_of(&reports, HotkeyAction::ToggleGameCursor),
            BindingOutcome::Failed(reason) if reason.contains("another GameFrame action")
        ));
    }

    #[test]
    fn register_each_skips_empty_bindings_without_reporting_them() {
        let bindings = HotkeyBindings {
            toggle_borderless: Some("  ".to_string()),
            ..HotkeyBindings::default()
        };

        let (registered, reports) = register_each(&bindings, |_| Ok(()));

        assert!(registered.is_empty());
        assert!(reports.is_empty());
    }

    fn a_report(binding: &str, outcome: BindingOutcome) -> BindingReport {
        BindingReport {
            action: HotkeyAction::ToggleBorderless,
            binding: binding.to_string(),
            outcome,
        }
    }

    #[test]
    fn a_newly_failing_binding_is_announced_with_its_reason() {
        let now = [a_report(
            "Ctrl+F4",
            BindingOutcome::Failed("already in use by another program".to_string()),
        )];

        let notice = new_failure_notice(&[], &now).unwrap();

        assert!(notice.contains("Ctrl+F4"));
        assert!(notice.contains("toggle borderless"));
        assert!(notice.contains("already in use by another program"));
    }

    #[test]
    fn a_failure_that_was_already_reported_is_not_announced_again() {
        let failed = a_report("Ctrl+F4", BindingOutcome::Failed("x".to_string()));

        assert_eq!(
            new_failure_notice(std::slice::from_ref(&failed), std::slice::from_ref(&failed)),
            None
        );
        // A different combination failing for the same action is new.
        let rebound = a_report("Ctrl+F5", BindingOutcome::Failed("x".to_string()));
        assert!(new_failure_notice(&[failed], &[rebound]).is_some());
    }

    #[test]
    fn registered_bindings_produce_no_failure_notice() {
        let ok = a_report("Ctrl+F4", BindingOutcome::Registered);

        assert_eq!(new_failure_notice(&[], &[ok]), None);
    }

    #[test]
    fn report_records_the_binding_string_it_was_for() {
        let (_, reports) = register_each(&all_three(), |_| Ok(()));

        assert_eq!(
            reports
                .iter()
                .find(|r| r.action == HotkeyAction::ToggleBorderless)
                .unwrap()
                .binding,
            "Ctrl+Alt+B"
        );
    }

    #[test]
    fn already_registered_is_described_as_in_use_by_another_program() {
        let hotkey: HotKey = "Ctrl+Alt+B".parse().unwrap();

        let reason = registration_failure_reason(&global_hotkey::Error::AlreadyRegistered(hotkey));

        assert_eq!(reason, "already in use by another program");
    }

    #[test]
    fn other_os_errors_are_described_as_an_os_refusal() {
        let err = global_hotkey::Error::OsError(std::io::Error::other("boom"));

        assert!(registration_failure_reason(&err).starts_with("the OS refused the hotkey"));
    }
}
