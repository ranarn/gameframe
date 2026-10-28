//! On-screen feedback for a user action that failed ("Apply to Window" refused,
//! the borderless hotkey on an untracked window, ...). The `tray-icon` crate has
//! no balloon API, so the notice goes through `Shell_NotifyIconW` with
//! `NIF_INFO` on the tray icon's own data -- non-blocking, so it is safe from a
//! hook or event callback on the UI thread. When the OS refuses the balloon, a
//! warning box on a worker thread says the same, so the failure is never only
//! in the log.

use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::Shell::{
    NIF_INFO, NIIF_WARNING, NIM_MODIFY, NOTIFYICONDATAW, Shell_NotifyIconW,
};

const TITLE: &str = "GameFrame";

/// Copies `text` into the fixed buffer `dst` (fixed-size, NUL-terminated
/// UTF-16). Text that does not fit is cut and ends in an ellipsis, so the
/// reader can tell it was shortened; a surrogate pair is never split.
fn fill_wide(dst: &mut [u16], text: &str) {
    let max = dst.len().saturating_sub(1);
    let units: Vec<u16> = text.encode_utf16().collect();
    let written = if units.len() <= max {
        dst[..units.len()].copy_from_slice(&units);
        units.len()
    } else if max > 0 {
        let mut keep = max - 1;
        // Do not end on the high half of a surrogate pair.
        if keep > 0 && (0xD800..0xDC00).contains(&units[keep - 1]) {
            keep -= 1;
        }
        dst[..keep].copy_from_slice(&units[..keep]);
        dst[keep] = 0x2026; // ellipsis
        keep + 1
    } else {
        0
    };
    dst[written..].fill(0);
}

/// Whether a fallback message box is open. At most one at a time, so repeated
/// failing presses cannot stack boxes.
static FALLBACK_BOX_OPEN: AtomicBool = AtomicBool::new(false);

/// Claims `flag`; `false` when it was already claimed.
fn try_claim(flag: &AtomicBool) -> bool {
    !flag.swap(true, Ordering::AcqRel)
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
    if !try_claim(&FALLBACK_BOX_OPEN) {
        log!("notice: a message box is already open; not opening another");
        return;
    }
    let text = text.to_string();
    // Detached on purpose: the box may stay open and must not stall the pump.
    std::thread::spawn(move || {
        crate::message_box::show_warning_box(&text);
        FALLBACK_BOX_OPEN.store(false, Ordering::Release);
    });
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
    fn fill_wide_truncates_with_an_ellipsis_and_keeps_the_terminator() {
        let mut buf = [0xffffu16; 4];

        fill_wide(&mut buf, "abcdef");

        assert_eq!(text_of(&buf), "ab\u{2026}");
        assert_eq!(buf[3], 0);
    }

    #[test]
    fn fill_wide_text_that_exactly_fits_is_not_marked_truncated() {
        let mut buf = [0xffffu16; 4];

        fill_wide(&mut buf, "abc");

        assert_eq!(text_of(&buf), "abc");
    }

    #[test]
    fn fill_wide_never_splits_a_surrogate_pair() {
        let mut buf = [0xffffu16; 4];

        // "a" + U+1F600 (two units) + "b": the cut would land mid-pair.
        fill_wide(&mut buf, "a\u{1F600}b");

        assert_eq!(text_of(&buf), "a\u{2026}");
    }

    #[test]
    fn only_the_first_claim_on_the_fallback_box_succeeds() {
        let flag = AtomicBool::new(false);

        assert!(try_claim(&flag));
        assert!(!try_claim(&flag));
        flag.store(false, Ordering::Release);
        assert!(try_claim(&flag));
    }

    #[test]
    fn fill_wide_keeps_non_ascii_text_intact() {
        let mut buf = [0u16; 16];

        fill_wide(&mut buf, "Zonen får plats");

        assert_eq!(text_of(&buf), "Zonen får plats");
    }
}
