//! On-screen feedback for a user action that failed ("Apply to Window" refused,
//! the borderless hotkey on an untracked window, ...). The `tray-icon` crate has
//! no balloon API, so the notice goes through `Shell_NotifyIconW` with
//! `NIF_INFO` on the tray icon's own data -- non-blocking, so it is safe from a
//! hook or event callback on the UI thread. When the OS refuses the balloon, a
//! warning box on a worker thread says the same, so the failure is never only
//! in the log.

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::Shell::{
    NIF_INFO, NIIF_WARNING, NIM_MODIFY, NOTIFYICONDATAW, Shell_NotifyIconW,
};

const TITLE: &str = "GameFrame";

/// Copies `text` into the fixed buffer `dst`, truncated so a terminating NUL
/// always fits (the balloon fields are fixed-size, NUL-terminated UTF-16).
fn fill_wide(dst: &mut [u16], text: &str) {
    let max = dst.len().saturating_sub(1);
    let mut written = 0;
    for unit in text.encode_utf16().take(max) {
        dst[written] = unit;
        written += 1;
    }
    dst[written..].fill(0);
}

/// Shows `text` as a warning balloon on the tray icon `(hwnd, uid)`. Returns
/// `false` when the shell did not take it.
fn balloon(hwnd: HWND, uid: u32, text: &str) -> bool {
    let mut data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: uid,
        uFlags: NIF_INFO,
        dwInfoFlags: NIIF_WARNING,
        ..Default::default()
    };
    fill_wide(&mut data.szInfo, text);
    fill_wide(&mut data.szInfoTitle, TITLE);
    // SAFETY: `data` is a fully initialised NOTIFYICONDATAW with the right
    // `cbSize` that outlives the call.
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) != 0 }
}

/// Tells the user `text`: a balloon on the tray icon `(hwnd, uid)`, else a
/// warning box on a worker thread. Never blocks the caller.
pub(crate) fn notify(hwnd: HWND, uid: u32, text: &str) {
    log!("notice: {text}");
    if balloon(hwnd, uid, text) {
        return;
    }
    log!("notice: the tray balloon was refused; using a message box");
    let text = text.to_string();
    // Detached on purpose: the box may stay open and must not stall the pump.
    std::thread::spawn(move || crate::message_box::show_warning_box(&text));
}

/// A warning box for a failure with no tray to show a balloon from (start-up).
/// Blocks until dismissed.
pub(crate) fn notify_blocking(text: &str) {
    crate::message_box::show_warning_box(text);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(buf: &[u16]) -> String {
        let end = buf.iter().position(|&u| u == 0).unwrap_or(buf.len());
        String::from_utf16(&buf[..end]).unwrap()
    }

    #[test]
    fn fill_wide_copies_short_text_and_terminates_it() {
        let mut buf = [0xffffu16; 8];

        fill_wide(&mut buf, "abc");

        assert_eq!(text_of(&buf), "abc");
        assert_eq!(buf[3..], [0; 5]);
    }

    #[test]
    fn fill_wide_truncates_to_leave_room_for_the_terminator() {
        let mut buf = [0xffffu16; 4];

        fill_wide(&mut buf, "abcdef");

        assert_eq!(text_of(&buf), "abc");
        assert_eq!(buf[3], 0);
    }

    #[test]
    fn fill_wide_keeps_non_ascii_text_intact() {
        let mut buf = [0u16; 16];

        fill_wide(&mut buf, "Zonen får plats");

        assert_eq!(text_of(&buf), "Zonen får plats");
    }
}
