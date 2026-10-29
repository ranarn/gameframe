//! A named, per-user (`Local\`) mutex for work that must not run in two
//! GameFrame processes at once: the tray process and its Settings child share
//! the config file and the update installer (#186).
//!
//! Windows mutexes belong to the thread that took them, so a guard must be
//! dropped on the thread that acquired it (the guard is not `Send`). A holder
//! that exits or dies leaves the mutex abandoned; the next waiter simply gets it.
//! Creating the mutex failing is not fatal: the guard then holds nothing
//! (failing open beats blocking the work).

use std::time::Duration;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};

/// Holds a named mutex until dropped. Not `Send`: see the module docs.
#[must_use]
pub struct NamedMutexGuard {
    /// Null when the mutex could not be created (nothing to release).
    handle: HANDLE,
    held: bool,
}

/// Takes the mutex `name` (`Local\...`), waiting up to `timeout`. `None` if
/// another thread or process still holds it after that. The same thread can
/// take it again while it holds it.
pub fn lock(name: &str, timeout: Duration) -> Option<NamedMutexGuard> {
    let wide = crate::wide::encode_wide(name);
    // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives the call;
    // null security attributes are valid.
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
    if handle.is_null() {
        log!("couldn't create the mutex {name}; continuing without it");
        return Some(NamedMutexGuard {
            handle,
            held: false,
        });
    }
    let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
    // SAFETY: `handle` is the valid mutex handle just created.
    let waited = unsafe { WaitForSingleObject(handle, millis) };
    if waited == WAIT_OBJECT_0 || waited == WAIT_ABANDONED {
        Some(NamedMutexGuard { handle, held: true })
    } else {
        // SAFETY: `handle` is valid and owned by this function.
        unsafe { CloseHandle(handle) };
        None
    }
}

impl Drop for NamedMutexGuard {
    fn drop(&mut self) {
        if self.handle.is_null() {
            return;
        }
        // SAFETY: `handle` is valid; the release happens on the acquiring
        // thread because the guard is not `Send`.
        unsafe {
            if self.held {
                ReleaseMutex(self.handle);
            }
            CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    fn unique_name(tag: &str) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        format!("Local\\GameFrame-test-{tag}-{}-{nanos}", std::process::id())
    }

    #[test]
    fn a_second_thread_cannot_take_a_held_mutex_until_it_is_released() {
        let name = unique_name("held");
        let first = lock(&name, Duration::ZERO).expect("free at first");

        let other = {
            let name = name.clone();
            std::thread::spawn(move || lock(&name, Duration::from_millis(30)).is_some())
        };
        assert!(!other.join().unwrap(), "held by the first thread");

        drop(first);
        let again = {
            let name = name.clone();
            std::thread::spawn(move || lock(&name, Duration::from_millis(500)).is_some())
        };
        assert!(again.join().unwrap(), "free after the release");
    }

    #[test]
    fn the_holding_thread_can_take_it_again() {
        let name = unique_name("recursive");
        let _outer = lock(&name, Duration::ZERO).expect("free");
        assert!(lock(&name, Duration::ZERO).is_some());
    }

    #[test]
    fn a_mutex_whose_holder_died_without_releasing_can_be_taken() {
        let name = unique_name("abandoned");
        let (tx, rx) = channel();
        let holder = {
            let name = name.clone();
            std::thread::spawn(move || {
                // Leaked on purpose: the thread ends still owning the mutex.
                std::mem::forget(lock(&name, Duration::ZERO).expect("free"));
                tx.send(()).unwrap();
            })
        };
        rx.recv().unwrap();
        holder.join().unwrap();

        assert!(lock(&name, Duration::from_millis(500)).is_some());
    }
}
