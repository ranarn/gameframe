## Manual taskbar-autohide hotkey used to desync from reality (fixed 2026-09-04)

### The bug
`win32::toggle_taskbar_hide` used to decide hide-vs-show from a caller-owned
`Option<TaskbarHideRestore>` flag (`app::run_event_loop`'s local
`taskbar_hide_state`), remembering only what *that hotkey itself* last did.
But the taskbar can also be hidden by a completely different, independent
call site: a tracked game's launch-detection auto-hide
(`app::handle_auto_apply_launch` → `win32::set_taskbar_hide_absolute_armed`),
which per `mem:gotchas/launch_watcher_winevent_hook`'s empirical validation
**fires on every real foreground-window change, not just the initial
launch** — so it re-arms/re-hides every time the user alt-tabs back into a
tracked game with `taskbar_autohide = true`.

Because that path never read or wrote the hotkey's own `taskbar_hide_state`
(by design — see `docs/adr/0002-taskbar-autohide-coupled-to-preset-apply.md`),
the two could disagree about whether the taskbar was actually hidden. Real
user report: pressing the hotkey while a game had already auto-hidden the
taskbar did nothing visible (the hotkey thought it was showing a hidden
taskbar, but was actually re-hiding an already-hidden one); the *next* press
then did the opposite of what was expected, and so on — the on/off sense was
permanently one step out of phase with reality.

### The fix
`toggle_taskbar_hide(mode)` no longer takes or keeps any remembered state at
all. It calls the new `is_taskbar_hidden(mode)` (reads `IsWindowVisible` on
`Shell_TrayWnd` for `FullyHidden`, or the real appbar auto-hide flag via
`read_appbar_flags`/`state_from_flags` for `HoverReveal`) to ground its
hide-vs-show decision in the taskbar's actual current OS-level state on
every press, then calls the existing absolute primitive
(`set_taskbar_hide_absolute`) with the opposite value. This does **not**
violate ADR 0002's independence requirement — it still never reads
`tracked_games`/config state or the `ARMED_TASKBAR_RESTORE` armed-window
registry, only the taskbar's real visibility — it's just no longer
independent of *reality*.

`app::run_event_loop`'s local `taskbar_hide_state` variable and
`handle_toggle_taskbar_autohide`'s second parameter were removed entirely as
part of this fix (no state left to thread through).

### The general lesson
A hotkey/toggle whose "on/off" decision is driven by a remembered
in-process flag will silently drift out of sync with reality the moment
*any other code path* can change the same real-world state without going
through that flag. If such a path exists (as launch-detection auto-hide does
here), ground the toggle's decision in the actual observed state instead of
trusting the flag — the flag is only ever a cache of what *this* call site
did, never a source of truth for what's actually true. Worth checking for
the same shape of bug anywhere else GameFrame keeps an `Option<Restore>`-style
toggle memory alongside an independent apply-path that can change the same
OS state (the borderless-toggle's own `RestoreState` in `toggle_state`
is the other candidate, but per ADR 0002/the borderless-toggle's own coupling
decision, tracked-game apply *does* intentionally write into that same state
via the coupling, so it's a different, deliberately-coupled situation, not
the same silent-drift bug).

### Test coverage
`win32::tests::toggle_taskbar_hide_reflects_the_real_state_regardless_of_who_hid_it_last`
(OS-facing, `#[ignore]`, mirrors the other real-taskbar tests in the same
module) simulates the exact game-launch-then-hotkey sequence against the
real `Shell_TrayWnd` and asserts the toggle gets both directions right with
no prior state passed in.
