//! The error type of every live Win32 window-manipulation call.

/// Reasons a live Win32 window-manipulation call can fail.
#[derive(Debug)]
pub enum Win32Error {
    /// The window is minimized (`IsIconic`), so its bounding rectangle would
    /// be its off-screen minimized position (roughly `(-32000, -32000)`)
    /// rather than a real, restorable region. Returned by both
    /// [`super::restore_window`] and [`super::apply_borderless_to_window`].
    WindowMinimized,
    /// A Win32 call failed; wraps the OS error from `GetLastError`.
    Os(std::io::Error),
    /// An `HRESULT` failure that isn't a `FACILITY_WIN32` code (so it can't
    /// be safely reinterpreted as a plain Win32/`GetLastError`-style error
    /// code) -- see [`Win32Error::from_hresult`].
    Hresult(i32),
    /// `FindWindowW(L"Shell_TrayWnd", ...)` returned a null handle -- the
    /// taskbar's own window couldn't be located, so the taskbar-autohide
    /// toggle has nothing to send `SHAppBarMessage` to.
    TaskbarNotFound,
    /// The target window's thread is not responding (`IsHungAppWindow`), so
    /// no call into it was made; the next auto-apply, or the drift re-check
    /// the drift handler schedules ([`super::HUNG_RECHECK_INTERVAL`]), retries.
    WindowHung,
}

impl Win32Error {
    /// Converts a failing `HRESULT` (e.g. from `DwmSetWindowAttribute`) into
    /// a [`Win32Error`].
    ///
    /// `HRESULT` failure codes are not plain `GetLastError()`-style codes in
    /// general (e.g. `E_ACCESSDENIED` is `0x80070005`, not `5`) -- but codes
    /// in the `FACILITY_WIN32` family (the `0x8007xxxx` range) wrap a plain
    /// Win32 error code in their low 16 bits by construction, so those are
    /// safely unwrapped into [`Win32Error::Os`] (letting
    /// [`is_access_denied`](Self::is_access_denied) and `Display` treat them
    /// identically to a `GetLastError`-sourced failure). Anything outside
    /// that facility is kept as an opaque [`Win32Error::Hresult`] rather than
    /// mislabeled.
    pub fn from_hresult(hresult: i32) -> Self {
        const FACILITY_WIN32_MASK: u32 = 0xFFFF_0000;
        const FACILITY_WIN32: u32 = 0x8007_0000;
        let bits = hresult as u32;
        if bits & FACILITY_WIN32_MASK == FACILITY_WIN32 {
            Win32Error::Os(std::io::Error::from_raw_os_error((bits & 0xFFFF) as i32))
        } else {
            Win32Error::Hresult(hresult)
        }
    }

    /// Whether this error looks like an access-denied failure -- i.e. the
    /// manipulation failed because GameFrame isn't running elevated and the
    /// target window is (e.g. it belongs to a process running as
    /// administrator). This is the signal `app` uses to decide whether to
    /// prompt for elevation rather than just reporting the failure.
    pub fn is_access_denied(&self) -> bool {
        const ERROR_ACCESS_DENIED: i32 = 5;
        matches!(self, Win32Error::Os(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED))
    }

    /// Whether the call was skipped because the target window is hung. The
    /// skip is already logged once per window by the win32 layer, so callers
    /// leave it out of their own failure logs.
    pub fn is_window_hung(&self) -> bool {
        matches!(self, Win32Error::WindowHung)
    }
}

impl std::fmt::Display for Win32Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Win32Error::WindowMinimized => write!(f, "window is minimized"),
            Win32Error::WindowHung => write!(f, "window is not responding"),
            Win32Error::Os(e) => write!(f, "Win32 window manipulation failed: {e}"),
            Win32Error::Hresult(hr) => write!(
                f,
                "Win32 window manipulation failed: HRESULT {:#010x}",
                *hr as u32
            ),
            Win32Error::TaskbarNotFound => {
                write!(f, "could not find the taskbar window (Shell_TrayWnd)")
            }
        }
    }
}

impl std::error::Error for Win32Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Win32Error::WindowMinimized => None,
            Win32Error::WindowHung => None,
            Win32Error::Os(e) => Some(e),
            Win32Error::Hresult(_) => None,
            Win32Error::TaskbarNotFound => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn win32_error_window_hung_is_flagged_and_displays() {
        assert!(Win32Error::WindowHung.is_window_hung());
        assert!(!Win32Error::WindowMinimized.is_window_hung());
        assert!(!Win32Error::WindowHung.is_access_denied());
        assert_eq!(
            Win32Error::WindowHung.to_string(),
            "window is not responding"
        );
    }

    #[test]
    fn win32_error_is_access_denied_recognizes_error_code_5() {
        let err = Win32Error::Os(std::io::Error::from_raw_os_error(5));

        assert!(err.is_access_denied());
    }

    #[test]
    fn win32_error_is_access_denied_is_false_for_other_error_codes() {
        let err = Win32Error::Os(std::io::Error::from_raw_os_error(2)); // ERROR_FILE_NOT_FOUND

        assert!(!err.is_access_denied());
    }

    #[test]
    fn win32_error_implements_display_and_std_error() {
        fn assert_is_std_error<E: std::error::Error>(_: &E) {}

        let os_err = Win32Error::Os(std::io::Error::from_raw_os_error(5));

        assert_is_std_error(&os_err);
        assert!(
            os_err
                .to_string()
                .starts_with("Win32 window manipulation failed: ")
        );
    }

    #[test]
    fn win32_error_from_hresult_unwraps_a_win32_facility_hresult_into_a_plain_os_error() {
        const E_ACCESSDENIED: i32 = 0x8007_0005u32 as i32;

        let err = Win32Error::from_hresult(E_ACCESSDENIED);

        // A FACILITY_WIN32 HRESULT (the `0x8007xxxx` family) wraps a plain
        // Win32 error code (`ERROR_ACCESS_DENIED` == 5) in its low 16 bits;
        // unwrapping it is what lets `is_access_denied` recognize it.
        assert!(err.is_access_denied());
        assert!(matches!(err, Win32Error::Os(ref e) if e.raw_os_error() == Some(5)));
    }

    #[test]
    fn win32_error_from_hresult_keeps_a_non_win32_facility_hresult_as_an_opaque_hresult() {
        const E_FAIL: i32 = 0x8000_4005u32 as i32;

        let err = Win32Error::from_hresult(E_FAIL);

        // Not a FACILITY_WIN32 code, so it can't be safely reinterpreted as
        // a plain Win32 error -- kept as-is, and (correctly) not mistaken
        // for access-denied.
        assert!(!err.is_access_denied());
        assert!(matches!(err, Win32Error::Hresult(E_FAIL)));
    }

    #[test]
    fn win32_error_hresult_variant_implements_display_and_std_error() {
        fn assert_is_std_error<E: std::error::Error>(_: &E) {}

        let err = Win32Error::Hresult(0x8000_4005u32 as i32);

        assert_is_std_error(&err);
        assert!(err.to_string().contains("0x80004005"));
    }

    #[test]
    fn win32_error_window_minimized_is_not_access_denied() {
        assert!(!Win32Error::WindowMinimized.is_access_denied());
    }

    #[test]
    fn win32_error_window_minimized_implements_display_and_std_error() {
        fn assert_is_std_error<E: std::error::Error>(_: &E) {}

        let err = Win32Error::WindowMinimized;

        assert_is_std_error(&err);
        assert_eq!(err.to_string(), "window is minimized");
    }

    #[test]
    fn win32_error_taskbar_not_found_is_not_access_denied() {
        assert!(!Win32Error::TaskbarNotFound.is_access_denied());
    }

    #[test]
    fn win32_error_taskbar_not_found_implements_display_and_std_error() {
        fn assert_is_std_error<E: std::error::Error>(_: &E) {}

        let err = Win32Error::TaskbarNotFound;

        assert_is_std_error(&err);
        assert_eq!(
            err.to_string(),
            "could not find the taskbar window (Shell_TrayWnd)"
        );
    }
}
