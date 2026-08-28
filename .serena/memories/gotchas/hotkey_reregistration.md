## Gotcha: global hotkeys must be re-registered after Settings closes

`src/app.rs::run_event_loop` owns an `Option<Hotkeys>` for the lifetime of the
Win32 message loop. Because Settings (`TrayAction::ShowSettings`) runs
synchronously via `open_settings()` and can persist changed hotkey bindings
to disk via `profile_store::save`, the loop's `hotkeys` variable MUST be
reassigned (`hotkeys = register_hotkeys(config_dir)`) right after
`open_settings()` returns -- otherwise newly bound/changed/removed hotkeys
silently don't work until a full app restart. This was a real bug (fixed in
commit `659437a`, "fix: re-register global hotkeys after Settings closes").

Mechanics that made the fix safe:
- `Hotkeys` wraps a `GlobalHotKeyManager`; its `Drop` impl destroys the
  manager's hidden window. Per the `global-hotkey` crate's Windows
  `platform_impl` (confirmed by reading the vendored source at
  `~/.cargo/registry/src/.../global-hotkey-0.8.0/src/platform_impl/windows/mod.rs`),
  hotkeys are registered against that hidden window via `RegisterHotKey`,
  and Win32 automatically unregisters hotkeys tied to a window when that
  window is destroyed -- so simply reassigning `hotkeys` to drop the old
  value before building the new one is enough; no manual `unregister` calls
  needed, and there's no window where old+new registrations overlap.
- `run_event_loop`'s `hotkeys` param had to change from `Option<&Hotkeys>`
  (borrowed once from `run()`) to owned `Option<Hotkeys>` (`mut`) so the
  `ShowSettings` branch could reassign it.

Testing boundary this bug ran into: `Hotkeys::register` (the OS-facing
`GlobalHotKeyManager` registration) and `run_event_loop` itself (the real
Win32 message pump) are both explicitly excluded from this crate's automated
tests per their own doc comments -- a headless/CI sandbox can't provide a
live event loop, and registering real global hotkeys in test runs risks
flakiness/conflicts. The regression test added
(`app::tests::reloading_config_after_a_settings_style_save_resolves_the_newly_bound_hotkey`)
only covers the pure "reload config from disk + resolve_bindings picks up a
change" half -- it cannot regression-guard the actual call-site line inside
`run_event_loop`. If this class of bug recurs, that's the architectural gap:
there is no seam to test "does the event loop's ShowSettings branch actually
re-register" short of a live message loop.
