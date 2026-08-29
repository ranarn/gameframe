# Hotkey capture widget: egui input capture and global-hotkey HotKey construction

**Date:** 2026-08-28
**Status:** Answered

## Question

For GitHub issue #17 (GameFrame Settings, replacing the free-text hotkey fields with press-to-bind): can egui 0.36.1's input API detect a full keychord (modifiers + a regular key) while the Settings window is focused, and can the result be converted into a `global_hotkey::hotkey::HotKey` (crate `global-hotkey` 0.8.0) without going through `HotKey`'s `FromStr`/string parsing? Does egui's `Modifiers`/`Key` map cleanly onto `global-hotkey`'s `Modifiers`/`Code`? Is there prior art for a press-to-bind widget in egui's own examples? (UX questions — in-progress capture state, clearing a binding — are explicitly out of scope, left for the ticket's grilling.)

Resolved crate versions (from `Cargo.lock` in the repo root): `egui 0.36.1`, `egui-winit 0.36.1`, `global-hotkey 0.8.0`, `keyboard-types 0.7.0` (global-hotkey's `hotkey::Modifiers`/`hotkey::Code` are re-exports of `keyboard_types::{Modifiers, Code}`).

## Findings

### 1. egui 0.36.1's input API for keychord capture

Source: `egui-0.36.1/src/input_state/mod.rs`, `egui-0.36.1/src/data/input/event.rs`, `egui-0.36.1/src/data/key.rs`, `egui-0.36.1/src/data/input/modifiers.rs`.

`egui::Context::input(|i| ...)` (documented example in `modifiers.rs`: `let modifiers = ctx.input(|i| i.modifiers);`) gives access to `egui::InputState`, which exposes everything needed:

- `pub events: Vec<Event>` — in-order events received this frame.
- `pub modifiers: Modifiers` — modifier state at the start of the frame.
- `pub keys_down: HashSet<Key>` — keys currently held (repeats not counted separately here).
- `pub raw: RawInput` — the raw backend input for the frame.

The event that matters is `Event::Key { key, physical_key, pressed, repeat, modifiers }` (`event.rs` lines 37-70):
- `key: Key` — the *logical* key, keymap-aware (Dvorak-adjusted, etc.).
- `physical_key: Option<Key>` — the *physical* key, ignoring keymaps (doc: "this ignores keymaps... ​`eframe` does not (yet) implement this on web"). GameFrame is a native Win32 `eframe`/`egui-winit` app, so this is populated there.
- `pressed: bool`, `repeat: bool`.
- `modifiers: Modifiers` — modifier state *at the time of this specific event*, which is exactly what a press-to-bind widget wants (no need to separately track `InputState::modifiers` across frames).

Critically, egui 0.36.1's `Key` enum was extended with **physical, per-side modifier variants** specifically for this use case. From `key.rs` lines 192-197 (verified against the real upstream file at commit `4c1f2fae95475a40e524884ebb298bcb1714b08e` via a direct, non-AI-processed `Invoke-WebRequest` fetch of `raw.githubusercontent.com`, to rule out a poisoned local cache — content matched byte-for-byte):

> "Modifier keys (exposed as distinct left/right variants so that games and input-capture UIs can bind them independently). egui's `Modifiers` struct still collapses both sides for the common case (e.g. "Ctrl+C"); these variants are emitted only as physical `Event::Key` presses."

The variants: `Key::ShiftLeft/ShiftRight`, `ControlLeft/ControlRight`, `AltLeft/AltRight`, `SuperLeft/SuperRight`, plus `Key::IntlBackslash` for the ISO 102nd key. `egui-winit-0.36.1/src/lib.rs` (lines ~1644-1655) confirms these are actually wired from winit's `KeyCode::ShiftLeft` etc. — not just declared and unused.

Practical implication for a capture widget: watch `ctx.input(|i| i.events.clone())` for `Event::Key { pressed: true, physical_key: Some(k), modifiers, .. }` where `k` is **not** one of the eight physical modifier variants — that's the "main" key of the chord, and `modifiers` on that same event gives the concurrently-held modifier state. No multi-frame state machine is strictly required beyond "wait for the first non-modifier `Event::Key` press."

egui also has its own `KeyboardShortcut` type (`data/input/keyboard_shortcut.rs`): `{ modifiers: Modifiers, logical_key: Key }`, with a `format()` method that round-trips to strings like `"Ctrl+Shift+F"`. This is egui's own developer-defined-shortcut mechanism (for in-app accelerators via `InputState::consume_shortcut`), not global OS hotkeys — but structurally it's the same shape (modifiers + one key) GameFrame would want to capture, and its string format is visually close to what `HotkeyBindings` already stores.

**Not verified:** whether an `egui::TextEdit` widget (as currently used in `settings_ui.rs`'s hotkey fields) would itself consume/interpret `Event::Key` before a capture handler could see it. This wasn't directly needed to answer the question (a press-to-bind implementation would presumably replace the `TextEdit` with a plain capture-zone widget, not layer capture logic on top of the existing `TextEdit::singleline`), so it was not chased down further — flagged as an open question below rather than guessed at.

### 2. global-hotkey 0.8.0's `HotKey` constructor

Source: `global-hotkey-0.8.0/src/hotkey.rs` (cached locally, read directly).

`HotKey` has a direct constructor that takes modifiers + a key code, no string involved:

```rust
pub struct HotKey {
    pub mods: Modifiers,
    pub key: Code,
    pub id: u32,
}

impl HotKey {
    pub fn new(mods: Option<Modifiers>, key: Code) -> Self { ... }
}
```

`mods` and `key` are public fields, `id()` is computed inside `new()` (`(mods.bits() << 16) | key as u32`), matching what `FromStr`-parsed instances get. `HotKey::new(Some(Modifiers::SHIFT), Code::KeyQ)` is a doc example directly in the crate's module docs. `Modifiers` and `Code` are **re-exports** (`pub use keyboard_types::{Code, Modifiers};`) from `keyboard-types 0.7.0` — not types local to `global-hotkey`.

`HotKey::new` auto-normalizes `Modifiers::META` to `Modifiers::SUPER` internally, so a caller doesn't need to special-case that translation.

**Display/round-trip:** `HotKey` implements `Display` via `into_string()` (lines 116-140), which produces lower-case, spelled-out strings like `"shift+control+alt+super+keyx"` — e.g. `Code::KeyB` becomes `"keyb"`, not `"B"`. `HotKey`'s own `FromStr`/`parse_key` is case-insensitive and accepts both long and short modifier names (`"ctrl"`/`"control"`, `"cmd"`/`"super"`/`"command"`) and both `"KeyB"` and `"B"` for letter keys, so `hotkey.to_string().parse::<HotKey>()` round-trips to an *equal* `HotKey` — but the stored string would **not** be byte-identical to GameFrame's current free-text convention (`"Ctrl+Alt+B"`). If GameFrame kept storing a string in `profile_store::HotkeyBindings` (rather than switching the stored representation), the `Display` impl is directly usable for that, but the persisted format would visibly change from what a user previously typed (e.g. `"Ctrl+Alt+B"` re-saved via capture-and-`Display` would come back as `"control+alt+keyb"`).

Confirmed by unit tests in the same file (`test_parse_hotkey`, `test_equality`), which is where the case-insensitivity and equality behavior were verified, not by inference from prose docs.

### 3. Cross-reference: egui `Modifiers`/`Key` vs global-hotkey `Modifiers`/`Code`

**Modifiers:** egui's `Modifiers` (`data/input/modifiers.rs`) has `alt`, `ctrl`, `shift`, `mac_cmd`, `command` (plain bools, with `command` as a cross-platform "ctrl or ⌘" convenience field). `keyboard_types::Modifiers` (`keyboard-types-0.7.0/src/modifiers.rs`) is a `bitflags!` type: `ALT`, `ALT_GRAPH`, `CAPS_LOCK`, `CONTROL`, `FN`, `FN_LOCK`, `META`, `NUM_LOCK`, `SCROLL_LOCK`, `SHIFT`, `SYMBOL`, `SYMBOL_LOCK`, `SUPER`. The four that matter for a hotkey (`alt`/`ctrl`/`shift`/super-or-cmd) translate directly: `egui::Modifiers::alt → Modifiers::ALT`, `.ctrl → Modifiers::CONTROL`, `.shift → Modifiers::SHIFT`, and Mac's `.mac_cmd`/Windows-Linux's `.ctrl`-as-`.command` both collapse to `Modifiers::SUPER`/`Modifiers::CONTROL` depending on platform — a small if/else, not a structural gap. egui has no lock-state modifiers (`caps_lock` etc.) in `Modifiers`, but those aren't relevant to a hotkey chord anyway.

**Key vs Code — same underlying model, different naming, one real gap:** `keyboard_types::Code` (`code.rs`) is explicitly a *physical position* enum modeled on the W3C UI Events `code` spec — the same concept as egui's `physical_key: Option<Key>`, not egui's logical `key: Key`. Comparing physical-to-physical:
- Letters/digits: clean 1:1 (`egui::Key::A ↔ Code::KeyA`, `Key::Num0 ↔ Code::Digit0`) — naming convention differs (`A` vs `KeyA`) but it's a straightforward translation table, not a gap.
- The eight physical modifier keys match exactly, **except one naming mismatch**: egui calls the OS/Windows key `SuperLeft`/`SuperRight`; `keyboard_types::Code` calls the same physical key `MetaLeft`/`MetaRight`. (Note `keyboard_types::Modifiers` itself uses `SUPER`, not `META`, for the bitflag — so the flag name matches egui's convention while the `Code` variant name doesn't. This asymmetry is a real, if minor, gotcha to be aware of when writing a translation table.)
- `IntlBackslash` matches by name on both sides.
- Function keys: `Code` goes to `F35`, matching egui's `Key` up to `F35` — no gap in the *enum*. However, `global-hotkey`'s own **string parser** (`parse_key` in `hotkey.rs`, lines 233-356) only recognizes `"F1"`..`"F24"` — it simply never added match arms for `F25`-`F35`, `NumpadBackspace`, `NumpadClear`, and most of the Sun-keyboard/media-key `Code` variants that do exist in the enum. This is a gap in the **crate's string-parsing surface only** — irrelevant if constructing `HotKey::new(mods, Code::F30)` directly, which is exactly the kind of gap a direct-construction approach sidesteps.
- `Copy`/`Cut`/`Paste`/`BrowserBack` all exist as `Code` variants too (matching egui's `Key::Copy/Cut/Paste/BrowserBack`), so no gap there.
- **The one real representational gap:** egui's logical `Key` enum has several "shifted punctuation" variants that are *logical*, not physical — `Colon`, `Pipe`, `Questionmark`, `Exclamationmark`, `OpenCurlyBracket`, `CloseCurlyBracket`, `Plus`. These exist because on a US layout, e.g., `Shift+Semicolon` *produces* `:`, and egui's logical `key` field reports that as `Key::Colon`. `keyboard_types::Code` has no `Colon`/`Pipe`/etc. variant at all — because `Code` is physical-position-only, and the physical key in question is just `Semicolon` (pressed with Shift). As long as a capture widget reads `physical_key` (as it should, to match `Code`'s physical model) rather than the logical `key`, this "gap" doesn't actually bite — `Semicolon`+Shift is exactly `Code::Semicolon` + `Modifiers::SHIFT`. It would only be a problem if the widget used the *logical* `key` field, which has no matching `Code` variant to translate to.

Net: egui's `physical_key`/`Modifiers` and global-hotkey's `Code`/`Modifiers` are close to a clean field-for-field mapping once you (a) use egui's `physical_key`, not `key`, and (b) rename `Super↔Meta` for the `Code` (not `Modifiers`) side. No egui-reportable physical key was found with zero `Code` counterpart, and no gap survives once direct `HotKey::new` construction is used instead of `HotKey::from_str`.

### 4. Prior art for a press-to-bind widget

**Not found in cached source.** GameFrame's `Cargo.lock`/local cargo registry cache includes `egui`, `egui-winit`, `egui_glow`, `egui-wgpu` at 0.35.1/0.36.1, but **not** `egui_demo_lib` (the crate containing egui's own demo app, including its "Input Event History" window referenced in `key.rs`'s own comments) — GameFrame doesn't depend on it, so it was never downloaded. No local example of a key-capture/press-to-bind widget was found in the egui core crate itself (it has no such widget built in — `TextEdit`, `Button`, etc. are the available primitives; nothing named `KeybindWidget` or similar exists in `egui-0.36.1`'s own source).

Falling back to a secondary source (web search, explicitly noted as such per the research brief): several third-party crates implementing exactly this "press a key to capture a binding" pattern exist in the egui ecosystem — `egui-keybind` (github.com/zeozeozeo/egui-keybind, has an `examples/simple.rs`), `egui-bind` (github.com/ItsEthra/egui-bind), and `egui-keybinds`. These were found via `WebSearch`, not inspected directly (their source isn't in this project's cargo cache and wasn't fetched), so their existence establishes that the pattern is common practice, but no code from them was verified against primary source. Treat this as evidence the pattern is well-trodden, not as a verified implementation reference.

### 5. UX questions (out of scope for this research)

Per the research brief, the in-progress capture state (what the widget shows while waiting for a keypress) and how to clear an existing binding are UI/UX design decisions, not factual questions with a documented answer in the crate sources. No recommendation is made here — left for the ticket's grilling/interview follow-up.

## Sources

- [`egui-0.36.1/src/input_state/mod.rs`](file:///C:/Users/jonas/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/egui-0.36.1/src/input_state/mod.rs) — `InputState` fields: `events`, `modifiers`, `keys_down`, `raw`; `key_pressed`/`key_down` helpers.
- [`egui-0.36.1/src/data/input/event.rs`](file:///C:/Users/jonas/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/egui-0.36.1/src/data/input/event.rs) — `Event::Key { key, physical_key, pressed, repeat, modifiers }` definition and doc comments.
- [`egui-0.36.1/src/data/key.rs`](file:///C:/Users/jonas/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/egui-0.36.1/src/data/key.rs) — `Key` enum, including the physical per-side modifier variants (`ShiftLeft` etc.) and `IntlBackslash`, plus `Key::name()`/`from_name()`.
- [`egui-0.36.1/src/data/input/modifiers.rs`](file:///C:/Users/jonas/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/egui-0.36.1/src/data/input/modifiers.rs) — `Modifiers` struct (`alt`, `ctrl`, `shift`, `mac_cmd`, `command`) and matching semantics.
- [`egui-0.36.1/src/data/input/keyboard_shortcut.rs`](file:///C:/Users/jonas/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/egui-0.36.1/src/data/input/keyboard_shortcut.rs) — egui's own `KeyboardShortcut { modifiers, logical_key }` and its `format()`.
- [`egui-winit-0.36.1/src/lib.rs`](file:///C:/Users/jonas/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/egui-winit-0.36.1/src/lib.rs) — confirms winit `KeyCode::ShiftLeft`/etc. are actually wired to `egui::Key::ShiftLeft`/etc., not dead code.
- [`raw.githubusercontent.com/emilk/egui/4c1f2fae95475a40e524884ebb298bcb1714b08e/crates/egui/src/data/key.rs`](https://raw.githubusercontent.com/emilk/egui/4c1f2fae95475a40e524884ebb298bcb1714b08e/crates/egui/src/data/key.rs) — direct (non-AI-mediated) fetch used to confirm the local cargo cache for `egui-0.36.1` is unmodified/genuine upstream (matched byte-for-byte), including the exact git commit recorded in the crate's own `.cargo_vcs_info.json`.
- [`global-hotkey-0.8.0/src/hotkey.rs`](file:///C:/Users/jonas/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/global-hotkey-0.8.0/src/hotkey.rs) — `HotKey` struct, `HotKey::new(Option<Modifiers>, Code)`, `Display`/`into_string()`, `FromStr`/`parse_hotkey`/`parse_key`, and the crate's own round-trip/equality unit tests.
- [`keyboard-types-0.7.0/src/modifiers.rs`](file:///C:/Users/jonas/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/keyboard-types-0.7.0/src/modifiers.rs) — `Modifiers` bitflags definition (`ALT`, `CONTROL`, `SHIFT`, `SUPER`, `META`, etc.), the type `global_hotkey::hotkey::Modifiers` re-exports.
- [`keyboard-types-0.7.0/src/code.rs`](file:///C:/Users/jonas/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/keyboard-types-0.7.0/src/code.rs) — full `Code` enum (physical key positions, W3C UI Events `code` spec), the type `global_hotkey::hotkey::Code` re-exports.
- `D:\Development\Home\gameframe\Cargo.lock` — resolved versions: `egui 0.36.1`, `egui-winit 0.36.1`, `global-hotkey 0.8.0`, `keyboard-types 0.7.0`.
- `D:\Development\Home\gameframe\src\hotkeys.rs`, `D:\Development\Home\gameframe\src\settings_ui.rs` — read-only, for context on current string-based binding storage/parsing (not modified).
- WebSearch: "egui press to bind keybind capture widget rust example github" — secondary source, surfaced `egui-keybind`, `egui-bind`, `egui-keybinds` third-party crates as prior art; not independently verified against their source.
- WebSearch: "global-hotkey rust crate HotKey::new modifiers Code constructor docs.rs" — secondary source, used only to corroborate the `HotKey::new(Option<Modifiers>, Code)` signature already confirmed directly from the cached crate source.

## Open questions

- **UX design** (explicitly out of scope per the research brief): what the capture widget shows while waiting for a keypress ("Press a key..." state), and how an existing binding is cleared. Left for the ticket's grilling/interview follow-up — no recommendation made here.
- **Whether `egui::TextEdit` (as currently used for the hotkey fields in `settings_ui.rs`) would interfere with reading `Event::Key` for capture purposes.** Not chased down, since a press-to-bind implementation would likely replace `TextEdit::singleline` with a different widget rather than layering capture on top of it — but this wasn't verified against `TextEdit`'s source, so it's noted here rather than assumed.
- **No local prior art found**: `egui_demo_lib` (which contains egui's own demo app and its "Input Event History" window) is not in this project's dependency tree/cargo cache, so egui's own examples could not be inspected directly. Third-party crates (`egui-keybind`, `egui-bind`, `egui-keybinds`) were found via web search only, not inspected as source.
- The exact interaction between `physical_key: Option<Key>` population and eframe/egui-winit's native Windows backend (i.e., confirming it's *reliably* `Some(..)` rather than `None` in practice on Windows, beyond the doc comment's claim that only the web backend lacks it) was not empirically tested — no running GameFrame instance was exercised as part of this research task.
