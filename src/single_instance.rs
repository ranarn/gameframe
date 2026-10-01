//! Single-instance guard: a second `gameframe.exe` exits immediately instead
//! of running alongside the first (two tray icons, two keyboard hooks, two
//! WinEvent hooks fighting over the same windows).
//!
//! Uses a named mutex in the session's `Local\` namespace, so each logged-in
//! user gets their own instance. The handle is deliberately never closed --
//! the OS releases it when the process exits, which is exactly the lifetime
//! the guard needs.

use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows_sys::Win32::System::Threading::CreateMutexW;

/// Same GUID as the installer's `AppId`, so the name can't collide with
/// another app's mutex.
const MUTEX_NAME: &str = "Local\\GameFrame-5C28E192-0AB5-4FA5-8874-88144D010E3B";

/// Returns `true` if this process is the only running GameFrame, `false` if
/// another instance already holds the mutex. If the mutex can't be created
/// at all, returns `true` -- failing open beats refusing to start.
pub fn acquire() -> bool {
    let name: Vec<u16> = MUTEX_NAME.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `name` is a NUL-terminated UTF-16 string that outlives the
    // call; null security attributes are valid. GetLastError is read
    // immediately after, before any other API call can overwrite it.
    let (handle, last_error) = unsafe {
        let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        (handle, GetLastError())
    };
    handle.is_null() || last_error != ERROR_ALREADY_EXISTS
}
