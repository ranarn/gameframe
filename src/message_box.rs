//! Modal message boxes titled "GameFrame", shared by the tray's update flow
//! ([`crate::app`]) and the install flow ([`crate::update_ui`]).

use windows_sys::Win32::UI::WindowsAndMessaging::{
    MB_ICONWARNING, MB_OK, MB_SETFOREGROUND, MessageBoxW,
};

/// Shows a message box titled "GameFrame" and returns the button pressed.
/// Blocks, so call it from a worker thread.
pub(crate) fn show_message_box(text: &str, style: u32) -> i32 {
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let text = wide(text);
    let title = wide("GameFrame");
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            style | MB_SETFOREGROUND,
        )
    }
}

/// Shows `text` in a warning box and waits for it to be dismissed.
pub(crate) fn show_warning_box(text: &str) {
    show_message_box(text, MB_OK | MB_ICONWARNING);
}
