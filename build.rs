//! Embeds `assets/icon.ico` as the compiled exe's Windows resource icon --
//! what Explorer, Alt-Tab, and the taskbar show for GameFrame.exe itself,
//! distinct from the tray icon and Settings window icon rendered at
//! runtime by `src/icon.rs` (those three are kept visually identical, but
//! the exe resource icon can only come from a build-time `.ico` file --
//! see `examples/gen_icon.rs` for how `assets/icon.ico` is produced).

fn main() {
    let mut resource = winresource::WindowsResource::new();
    resource.set_icon("assets/icon.ico");
    resource.compile().expect("failed to embed the Windows app icon resource (assets/icon.ico)");
}
