## Issue #40: Ctrl+Escape as a hotkey-capture combo + Start-menu suppression

Two independent problems fixed together:
1. `SettingsState::handle_key_event` (src/settings_ui.rs) used to cancel capture on ANY
   `Escape` press. Fixed to only cancel when `modifiers.is_none()` (`key ==
   egui::Key::Escape && modifiers.is_none()`) -- Escape+modifier now falls through to
   `format_captured_hotkey` like any other key.
2. `key_display_name`/`key_to_code` (both in settings_ui.rs) previously excluded `Escape`
   entirely (routed to the catch-all `_ => return None`). Now `Escape` has real entries
   (`"Escape"` / `Code::Escape`) in both, reachable only when a modifier was held (an
   unmodified Escape never reaches `format_captured_hotkey` at all, intercepted earlier).

New module `src/keyboard_hook.rs` (registered in main.rs, binary-only module -- NOT in
lib.rs, so its tests only run under `cargo test --bin gameframe`, not `--lib`), split the
same pure/OS-facing way as `win32.rs`/`hotkeys.rs`:
- `should_suppress(vk_code: u32, ctrl_held: bool) -> bool` -- pure predicate, unit tested.
  True only for VK_ESCAPE (0x1B) + ctrl_held.
- `KeyboardHookGuard` -- OS-facing RAII wrapper around a real `WH_KEYBOARD_LL` hook
  (`SetWindowsHookExW`/`UnhookWindowsHookEx`), untested by convention (mirrors
  `hotkeys::Hotkeys`/`zone_overlay::TaskbarRestoreGuard`'s existing Drop-based pattern).
  `install()` returns `Option<Self>` (None on failure, treated as "no suppression this
  session" rather than a hard error). `Drop` doesn't run under `panic = "abort"`, but
  that's fine -- Windows unregisters all of a process's hooks on termination regardless;
  commented at the impl so a future reader doesn't re-flag it.
- The hook proc reads Ctrl state via `GetAsyncKeyState(VK_CONTROL)` since
  `KBDLLHOOKSTRUCT` only carries the triggering key's own vkCode, not full modifier state.
  Checks both `WM_KEYDOWN`/`WM_SYSKEYDOWN`.

### CRITICAL architectural gotcha (post-review fix, still in #40)
A `WH_KEYBOARD_LL` hook installed with `dwThreadId == 0` intercepts hardware input
*before* Windows turns it into a window message -- including for the hook-installing
process's own window. Since Settings receives keys through winit's normal message
pipeline, a suppressed (returned `1` without `CallNextHookEx`) Ctrl+Escape can NEVER
arrive as `egui::Event::Key` -- `hotkey_row`'s event loop / `handle_key_event` would never
see it, so capture would stay stuck "listening" forever despite AC1 (no Start menu) being
satisfied.

Fix: `keyboard_hook_proc` sets a module-private `static CTRL_ESCAPE_SUPPRESSED: AtomicBool`
the instant it swallows Ctrl+Escape (safe as plain global state -- only one
`KeyboardHookGuard`/hook exists at a time, and the proc runs synchronously on the same UI
thread that installed the hook and later polls the flag). `take_suppressed_ctrl_escape()`
(pure poll-and-reset via `swap(false, SeqCst)`, unit tested) is polled every
`SettingsApp::sync_keyboard_hook` call. `SettingsState::complete_capture_with_ctrl_escape_if_suppressed(bool)`
is the pure decision (unit tested, in settings_ui.rs) that -- given the already-polled flag
-- completes the capture directly by calling `self.handle_key_event(Modifiers{ctrl:true,..},
Key::Escape)`, reusing the existing tested Ctrl+Escape formatting/duplicate-check path
rather than duplicating it. No staleness risk: the flag can only be set while the hook is
installed, and the hook is only installed while `capturing.is_some()`, so it can never be
true while `capturing` is `None`.

Lifecycle wiring: `SettingsApp` (settings_ui.rs) has a `keyboard_hook:
Option<keyboard_hook::KeyboardHookGuard>` field, synced by `SettingsApp::sync_keyboard_hook`:
installs when `state.capturing.is_some()` and not already installed, polls+applies the
Ctrl+Escape flag, drops (`= None`) when `state.capturing` is `None`. Called TWICE per frame
from `eframe::App::ui`: once at the very top, and again immediately after both
`hotkey_row(...)` calls -- the second call closes a one-frame install gap (a "Set hotkey"
click sets `capturing = Some` mid-frame, after the first sync already ran against the old
`None` state; without the second call the hook wouldn't install until the *next* frame,
leaving a one-frame window where physical Ctrl+Escape still reaches the Start menu).
Covers all 3 required exit paths: successful capture and explicit cancel both go through
`cancel_capture`/`handle_key_event` setting `capturing = None`; the Settings window closing
drops `SettingsApp` itself (and the field with it) via ordinary Rust drop order.

Cargo.toml windows-sys features added: `Win32_System_LibraryLoader` (GetModuleHandleW) and
`Win32_UI_Input_KeyboardAndMouse` (VK_CONTROL/VK_ESCAPE/GetAsyncKeyState). `HHOOK` lives in
`Win32_UI_WindowsAndMessaging`, NOT `Win32_Foundation` (easy to guess wrong).

Known minor tradeoff (not fixed, out of scope for #40): if `KeyboardHookGuard::install()`
fails, `sync_keyboard_hook` retries it every call for the rest of that capture session
(since `self.keyboard_hook` stays `None`). Harmless but slightly wasteful; commented at the
retry site. Would need a separate "already tried and failed" flag to avoid.
