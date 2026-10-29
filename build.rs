//! Embeds `assets/icon.ico` as the compiled exe's Windows resource icon --
//! what Explorer, Alt-Tab, and the taskbar show for GameFrame.exe itself,
//! distinct from the tray icon and Settings window icon rendered at
//! runtime by `src/icon.rs` (those three are kept visually identical, but
//! the exe resource icon can only come from a build-time `.ico` file --
//! see `examples/gen_icon.rs` for how `assets/icon.ico` is produced).

fn main() {
    // Static imports resolve from System32 only (LOAD_LIBRARY_SEARCH_SYSTEM32),
    // before `main` can call SetDefaultDllDirectories. Applies to binary targets
    // only (including the bin's test harness, where it is harmless), not the
    // library or examples; MSVC linker only.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo::rustc-link-arg-bins=/DEPENDENTLOADFLAG:0x800");
    }
    let mut resource = winresource::WindowsResource::new();
    resource.set_icon("assets/icon.ico");
    // Shown by Task Manager, Explorer's file properties and SmartScreen;
    // the defaults would be the lowercase package name.
    resource.set("ProductName", "GameFrame");
    resource.set("FileDescription", "GameFrame");
    resource.set("CompanyName", "Jonas Ranerfors");
    resource.set(
        "LegalCopyright",
        "Copyright (C) 2026 Jonas Ranerfors. Licensed under GPL-3.0-or-later.",
    );
    // Per-Monitor V2 DPI awareness from the first instruction, so every Win32
    // coordinate (monitor geometry, window rects, SetWindowPos) is in physical
    // pixels regardless of when winit/eframe first runs. `dpiAware` is the
    // fallback for pre-Windows-10-1703 loaders, which ignore `dpiAwareness`.
    resource.set_manifest(
        r#"<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2, PerMonitor</dpiAwareness>
    </windowsSettings>
  </application>
</assembly>"#,
    );
    resource
        .compile()
        .expect("failed to embed the Windows app icon resource (assets/icon.ico)");
}
