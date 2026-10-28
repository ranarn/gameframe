use super::*;
use windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible;

#[test]
fn armed_state_exposes_the_session_restore_and_not_after_the_session_ends() {
    let mut state = ArmedTaskbarWindows::default();
    assert_eq!(state.session_restore(), None);
    state.start(1, TaskbarHideRestore::FullyHidden);
    state.join(2);
    assert_eq!(
        state.session_restore(),
        Some(TaskbarHideRestore::FullyHidden)
    );
    state.take_all();
    assert_eq!(state.session_restore(), None);
}

/// The reference-counted armed-window state (pure -- no OS call, no
/// shared static touched, unlike `take_armed_taskbar_restore_for_window`
/// / `set_taskbar_hide_absolute_armed`, which wrap this same logic
/// around the real `ARMED_TASKBAR_RESTORE` static). The multi-window
/// scenario: two windows hide the taskbar, one closes (still hidden, no
/// restore yet), then the other closes (now restores with the *original*
/// pre-first-hide value, not whatever the second window's own arm would
/// have captured).
#[test]
fn armed_taskbar_windows_a_single_window_arming_then_leaving_restores_immediately() {
    let mut state = ArmedTaskbarWindows::default();
    let window = 1isize;

    state.start(window, TaskbarHideRestore::FullyHidden);
    let restore = state.leave(window);

    assert_eq!(restore, Some(TaskbarHideRestore::FullyHidden));
    assert!(!state.is_active());
}

#[test]
fn armed_taskbar_windows_two_windows_hiding_then_one_closing_keeps_the_taskbar_hidden() {
    let mut state = ArmedTaskbarWindows::default();
    let window_a = 1isize;
    let window_b = 2isize;
    state.start(window_a, TaskbarHideRestore::FullyHidden);
    state.join(window_b);

    let restore_after_a_closes = state.leave(window_a);

    assert_eq!(
        restore_after_a_closes, None,
        "window B still wants the taskbar hidden"
    );
    assert!(state.is_active());
}

#[test]
fn armed_taskbar_windows_restores_the_original_pre_first_hide_value_once_the_last_window_closes() {
    let mut state = ArmedTaskbarWindows::default();
    let window_a = 1isize;
    let window_b = 2isize;
    let original = TaskbarHideRestore::HoverReveal(TaskbarAutohideState::AlwaysVisible);
    state.start(window_a, original);
    state.join(window_b);
    state.leave(window_a);

    let restore_after_b_closes = state.leave(window_b);

    assert_eq!(restore_after_b_closes, Some(original));
    assert!(!state.is_active());
}

#[test]
fn armed_taskbar_windows_join_never_overwrites_the_remembered_restore_value() {
    // Regression test for the value-loss bug: joining an already-active
    // session must never re-capture/overwrite the true pre-first-hide
    // restore value.
    let mut state = ArmedTaskbarWindows::default();
    state.start(1, TaskbarHideRestore::FullyHidden);

    state.join(2);

    assert_eq!(state.restore, Some(TaskbarHideRestore::FullyHidden));
}

#[test]
fn armed_taskbar_windows_leave_of_an_unarmed_window_returns_none_and_leaves_the_session_untouched()
{
    let mut state = ArmedTaskbarWindows::default();
    state.start(1, TaskbarHideRestore::FullyHidden);

    let result = state.leave(99);

    assert_eq!(result, None);
    assert!(state.is_active());
    assert!(state.windows.contains(&1));
}

#[test]
fn armed_taskbar_windows_is_last_true_only_when_hwnd_is_the_sole_armed_window() {
    let mut state = ArmedTaskbarWindows::default();
    state.start(1, TaskbarHideRestore::FullyHidden);

    assert!(state.is_last(1));

    state.join(2);

    assert!(!state.is_last(1));
    assert!(!state.is_last(2));
}

#[test]
fn armed_taskbar_windows_take_all_clears_the_whole_session_and_returns_the_restore_value_regardless_of_how_many_windows_were_armed()
 {
    let mut state = ArmedTaskbarWindows::default();
    state.start(1, TaskbarHideRestore::FullyHidden);
    state.join(2);
    state.join(3);

    let restore = state.take_all();

    assert_eq!(restore, Some(TaskbarHideRestore::FullyHidden));
    assert!(!state.is_active());
}

#[test]
fn armed_taskbar_windows_take_all_returns_none_when_nothing_is_armed() {
    let mut state = ArmedTaskbarWindows::default();

    assert_eq!(state.take_all(), None);
}

/// Manual, real-OS verification for the `FullyHidden` mode (the taskbar
/// must not reappear when the mouse hovers near the bottom of the screen
/// while hidden -- not scriptable without OS-level screen/window-state
/// inspection, but the underlying `ShowWindow` state change *is*
/// directly observable via `IsWindowVisible`). Ignored by default (not part
/// of the automated suite, same OS-facing convention as the rest of this
/// module) -- run explicitly with `cargo test --lib taskbar::tests::hide_taskbar_completely_and_show_taskbar_actually_change_the_real_taskbars_window_visibility -- --ignored`
/// on a real desktop session. Mutates the real Shell_TrayWnd on this
/// machine for the duration of the test, restoring it in every case
/// (including a failed assertion, via a drop guard) so a test failure
/// never leaves the real taskbar hidden.
#[test]
#[ignore = "OS-facing: mutates the real taskbar window; run manually with --ignored"]
fn hide_taskbar_completely_and_show_taskbar_actually_change_the_real_taskbars_window_visibility() {
    struct RestoreOnDrop;
    impl Drop for RestoreOnDrop {
        fn drop(&mut self) {
            let _ = show_taskbar();
        }
    }
    let _restore = RestoreOnDrop;

    let hwnd = find_taskbar_window().expect("Shell_TrayWnd must exist on a real desktop session");

    hide_taskbar_completely()
        .expect("hide_taskbar_completely should succeed on a real desktop session");
    let visible_while_hidden = unsafe { IsWindowVisible(hwnd) };
    assert_eq!(
        visible_while_hidden, 0,
        "Shell_TrayWnd should be invisible after hide_taskbar_completely"
    );

    show_taskbar().expect("show_taskbar should succeed on a real desktop session");
    let visible_after_show = unsafe { IsWindowVisible(hwnd) };
    assert_ne!(
        visible_after_show, 0,
        "Shell_TrayWnd should be visible again after show_taskbar"
    );
}

/// Manual, real-OS verification for the auto-restore-on-close feature:
/// exercises the exact same two calls
/// `crate::launch_watcher`'s `EVENT_OBJECT_DESTROY` callback makes
/// (`take_armed_taskbar_restore_for_window` then `restore_taskbar`),
/// against the real taskbar, without needing a real second window whose
/// destroy event would have to actually fire -- `set_taskbar_hide_absolute_armed`/
/// `take_armed_taskbar_restore_for_window` never dereference the `HWND`
/// they're given, only store/compare its pointer-identity value (see
/// `ArmedTaskbarWindows`), so a fabricated `HWND` value stands in for
/// "the window that was hidden" perfectly well here.
///
/// Ignored by default (not part of the automated suite, same OS-facing
/// convention as the rest of this module) -- run explicitly with
/// `cargo test --bin gameframe taskbar::tests::set_taskbar_hide_absolute_armed_then_take_armed_taskbar_restore_for_window_restores_the_real_taskbar_on_a_simulated_window_close -- --ignored`
/// on a real desktop session. Mutates the real Shell_TrayWnd on this
/// machine for the duration of the test, restoring it in every case
/// (including a failed assertion, via a drop guard) so a test failure
/// never leaves the real taskbar hidden.
#[test]
#[ignore = "OS-facing: mutates the real taskbar window; run manually with --ignored"]
fn set_taskbar_hide_absolute_armed_then_take_armed_taskbar_restore_for_window_restores_the_real_taskbar_on_a_simulated_window_close()
 {
    struct RestoreOnDrop;
    impl Drop for RestoreOnDrop {
        fn drop(&mut self) {
            let _ = show_taskbar();
        }
    }
    let _restore = RestoreOnDrop;

    let taskbar_hwnd =
        find_taskbar_window().expect("Shell_TrayWnd must exist on a real desktop session");
    // A fabricated identity for "the game window that was hidden" --
    // never dereferenced, only compared (see doc comment above).
    let fake_game_hwnd = 0xDEAD_usize as HWND;

    set_taskbar_hide_absolute_armed(fake_game_hwnd, true, TaskbarHideMode::FullyHidden)
        .expect("hide should succeed on a real desktop session");
    let visible_while_hidden = unsafe { IsWindowVisible(taskbar_hwnd) };
    assert_eq!(
        visible_while_hidden, 0,
        "Shell_TrayWnd should be invisible after arming a hide for fake_game_hwnd"
    );

    // Simulate `window_destroy_callback`'s own logic exactly: take the
    // armed restore for the "closed" window's HWND, then apply it.
    let restore = take_armed_taskbar_restore_for_window(fake_game_hwnd)
        .expect("a restore should be armed for the window that was just hidden");
    restore_taskbar(restore).expect("restore_taskbar should succeed on a real desktop session");

    let visible_after_restore = unsafe { IsWindowVisible(taskbar_hwnd) };
    assert_ne!(
        visible_after_restore, 0,
        "Shell_TrayWnd should be visible again after the simulated window-close restore"
    );
}

/// Companion to the test above: an *unrelated* window closing (i.e. its
/// `HWND` doesn't match whatever is currently armed) must never trigger
/// a restore. Same real-OS caveats as the test above.
#[test]
#[ignore = "OS-facing: mutates the real taskbar window; run manually with --ignored"]
fn take_armed_taskbar_restore_for_window_leaves_an_unrelated_windows_hide_untouched() {
    struct RestoreOnDrop;
    impl Drop for RestoreOnDrop {
        fn drop(&mut self) {
            let _ = show_taskbar();
        }
    }
    let _restore = RestoreOnDrop;

    let taskbar_hwnd =
        find_taskbar_window().expect("Shell_TrayWnd must exist on a real desktop session");
    let armed_hwnd = 0xDEAD_usize as HWND;
    let unrelated_hwnd = 0xBEEF_usize as HWND;

    set_taskbar_hide_absolute_armed(armed_hwnd, true, TaskbarHideMode::FullyHidden)
        .expect("hide should succeed on a real desktop session");

    assert!(
        take_armed_taskbar_restore_for_window(unrelated_hwnd).is_none(),
        "an unrelated window's destroy event must not take the armed restore"
    );
    let visible_after_unrelated_destroy = unsafe { IsWindowVisible(taskbar_hwnd) };
    assert_eq!(
        visible_after_unrelated_destroy, 0,
        "Shell_TrayWnd should remain hidden -- an unrelated window's close must have no effect"
    );

    // Clean up: take the real armed restore and apply it (also
    // verifies the armed state survived the unrelated take attempt
    // above untouched).
    let restore = take_armed_taskbar_restore_for_window(armed_hwnd).expect(
        "the original armed restore should still be armed after the unrelated take attempt",
    );
    restore_taskbar(restore).expect("restore_taskbar should succeed on a real desktop session");
}

/// Real-OS test of the multi-window case: two tracked-game windows both hiding the
/// taskbar (GameFrame's central multi-game use case, e.g. `Left`+`Right`
/// side-by-side), then closing one at a time. The taskbar must stay
/// hidden after the first window closes (the second still wants it
/// hidden) and only actually restore once the second -- and last --
/// window closes. Same real-OS caveats as the tests above (fabricated
/// `HWND`s, never dereferenced -- see the doc comment on the test
/// above).
#[test]
#[ignore = "OS-facing: mutates the real taskbar window; run manually with --ignored"]
fn set_taskbar_hide_absolute_armed_keeps_the_real_taskbar_hidden_until_the_last_of_two_armed_windows_closes()
 {
    struct RestoreOnDrop;
    impl Drop for RestoreOnDrop {
        fn drop(&mut self) {
            let _ = show_taskbar();
        }
    }
    let _restore = RestoreOnDrop;

    let taskbar_hwnd =
        find_taskbar_window().expect("Shell_TrayWnd must exist on a real desktop session");
    let window_a = 0x1111_usize as HWND;
    let window_b = 0x2222_usize as HWND;

    set_taskbar_hide_absolute_armed(window_a, true, TaskbarHideMode::FullyHidden)
        .expect("hiding for window A should succeed on a real desktop session");
    set_taskbar_hide_absolute_armed(window_b, true, TaskbarHideMode::FullyHidden)
        .expect("hiding for window B (joining the already-active session) should succeed");
    let visible_while_both_hidden = unsafe { IsWindowVisible(taskbar_hwnd) };
    assert_eq!(
        visible_while_both_hidden, 0,
        "Shell_TrayWnd should be invisible while both windows are armed"
    );

    // Simulate window A closing: the destroy watcher's own logic.
    assert!(
        take_armed_taskbar_restore_for_window(window_a).is_none(),
        "window B is still armed, so window A closing must not yield a restore value yet"
    );
    let visible_after_a_closes = unsafe { IsWindowVisible(taskbar_hwnd) };
    assert_eq!(
        visible_after_a_closes, 0,
        "Shell_TrayWnd must remain hidden -- window B still wants it hidden (a premature restore is what this test guards against)"
    );

    // Now simulate window B closing -- the last armed window.
    let restore = take_armed_taskbar_restore_for_window(window_b)
        .expect("window B was the last armed window, so a restore value should now be returned");
    restore_taskbar(restore).expect("restore_taskbar should succeed on a real desktop session");

    let visible_after_b_closes = unsafe { IsWindowVisible(taskbar_hwnd) };
    assert_ne!(
        visible_after_b_closes, 0,
        "Shell_TrayWnd should be visible again once the last armed window closes"
    );
}

/// Manual, real-OS verification for the tray-"Exit" auto-restore path:
/// exercises the exact same two calls `crate::app::run` makes
/// after `run_event_loop` returns (`take_any_armed_taskbar_restore`
/// then `restore_taskbar`), against the real taskbar. Same real-OS
/// caveats as the tests above.
#[test]
#[ignore = "OS-facing: mutates the real taskbar window; run manually with --ignored"]
fn take_any_armed_taskbar_restore_then_restore_taskbar_restores_the_real_taskbar_regardless_of_which_window_was_armed()
 {
    struct RestoreOnDrop;
    impl Drop for RestoreOnDrop {
        fn drop(&mut self) {
            let _ = show_taskbar();
        }
    }
    let _restore = RestoreOnDrop;

    let taskbar_hwnd =
        find_taskbar_window().expect("Shell_TrayWnd must exist on a real desktop session");
    let fake_game_hwnd = 0xCAFE_usize as HWND;

    set_taskbar_hide_absolute_armed(fake_game_hwnd, true, TaskbarHideMode::FullyHidden)
        .expect("hide should succeed on a real desktop session");
    let visible_while_hidden = unsafe { IsWindowVisible(taskbar_hwnd) };
    assert_eq!(
        visible_while_hidden, 0,
        "Shell_TrayWnd should be invisible after arming a hide"
    );

    // Simulate `crate::app::run`'s own tray-"Exit" logic exactly: take
    // whatever is armed, regardless of which window it was armed for.
    let restore =
        take_any_armed_taskbar_restore().expect("a restore should be armed after the hide above");
    restore_taskbar(restore).expect("restore_taskbar should succeed on a real desktop session");

    let visible_after_restore = unsafe { IsWindowVisible(taskbar_hwnd) };
    assert_ne!(
        visible_after_restore, 0,
        "Shell_TrayWnd should be visible again after the simulated tray-Exit restore"
    );
}

/// Real-OS test: the manual taskbar-autohide hotkey
/// (`toggle_taskbar_hide`) decides hide-vs-show from the taskbar's real
/// state on every press, regardless of which call site hid it last (here
/// a tracked game's launch-detection auto-hide, simulated via
/// `set_taskbar_hide_absolute_armed`). The first press after such a hide
/// must show the taskbar and the next must hide it again, with no state
/// of its own passed in. Same real-OS caveats as the tests above.
#[test]
#[ignore = "OS-facing: mutates the real taskbar window; run manually with --ignored"]
fn toggle_taskbar_hide_reflects_the_real_state_regardless_of_who_hid_it_last() {
    struct RestoreOnDrop;
    impl Drop for RestoreOnDrop {
        fn drop(&mut self) {
            let _ = show_taskbar();
        }
    }
    let _restore = RestoreOnDrop;

    let taskbar_hwnd =
        find_taskbar_window().expect("Shell_TrayWnd must exist on a real desktop session");
    let fake_game_hwnd = 0xF00D_usize as HWND;

    // Simulate a tracked game's launch-detection auto-hide -- a call
    // site `toggle_taskbar_hide` never reads or writes state for.
    set_taskbar_hide_absolute_armed(fake_game_hwnd, true, TaskbarHideMode::FullyHidden)
        .expect("hide should succeed on a real desktop session");
    assert_eq!(
        unsafe { IsWindowVisible(taskbar_hwnd) },
        0,
        "Shell_TrayWnd should be hidden by the simulated game launch"
    );

    // The manual toggle, despite having no memory of that hide, must
    // still detect "currently hidden" from the real OS state and show it.
    toggle_taskbar_hide(TaskbarHideMode::FullyHidden)
        .expect("toggle should succeed on a real desktop session");
    assert_ne!(
        unsafe { IsWindowVisible(taskbar_hwnd) },
        0,
        "toggle should show the taskbar since it was actually hidden"
    );

    // And the very next press must detect "currently visible" and hide
    // it again.
    toggle_taskbar_hide(TaskbarHideMode::FullyHidden)
        .expect("toggle should succeed on a real desktop session");
    assert_eq!(
        unsafe { IsWindowVisible(taskbar_hwnd) },
        0,
        "toggle should hide the taskbar again since it was actually visible"
    );

    // Clean up the simulated game's armed-window bookkeeping (the
    // toggle above never touched it, so it's still armed).
    let _ = take_armed_taskbar_restore_for_window(fake_game_hwnd);
}
