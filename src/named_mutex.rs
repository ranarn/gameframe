//! A named, per-user (`Local\`) mutex for work that must not run in two
//! GameFrame processes at once: the tray process and its Settings child share
//! the config file and the update installer (#186).
//!
//! Windows mutexes belong to the thread that took them, so a guard must be
//! dropped on the thread that acquired it (the guard is not `Send`). A holder
//! that exits or dies leaves the mutex abandoned; the next waiter simply gets it.
//! Creating the mutex failing is not fatal: the guard then holds nothing
//! (failing open beats blocking the work).
//!
//! Squatting: any process of the session can create a `Local\` name first and
//! hold it, which would block updates or stall config writes for good. So the
//! mutex is created with a security descriptor that gives access to the current
//! user's SID only and a medium mandatory integrity label with no read/write/
//! execute up, which keeps lower-integrity processes (a sandboxed one) from
//! opening it. If the name already exists, the object must carry at least that
//! label, else it was pre-created by a lower-integrity process: it is ignored
//! (logged) and the guard holds nothing. `Local\` names are per session, not
//! per user, so an existing mutex must also be owned by the current user (ours
//! set the owner explicitly); one owned by another user of the session is
//! ignored the same way. A same-user, same-integrity process is
//! out of scope: it can write `config.toml` and run the installer itself.

use std::time::Duration;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, LocalFree, SetLastError,
    WAIT_ABANDONED, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetAce, GetKernelObjectSecurity, GetSecurityDescriptorOwner, GetSecurityDescriptorSacl,
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, LABEL_SECURITY_INFORMATION,
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
    SYSTEM_MANDATORY_LABEL_ACE, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, OpenProcessToken, ReleaseMutex, WaitForSingleObject,
};

/// The integrity level "medium" (`SECURITY_MANDATORY_MEDIUM_RID`) a mutex of
/// ours is labelled with; an object without a label counts as this too.
const MEDIUM_RID: u32 = 0x2000;

/// `SYSTEM_MANDATORY_LABEL_ACE_TYPE`.
const MANDATORY_LABEL_ACE_TYPE: u8 = 0x11;

/// Holds a named mutex until dropped. Not `Send`: see the module docs.
#[must_use]
pub struct NamedMutexGuard {
    /// Null when the mutex could not be created or was not trusted (nothing to
    /// release).
    handle: HANDLE,
    held: bool,
}

impl NamedMutexGuard {
    /// A guard that holds nothing: the work goes ahead unlocked.
    fn unlocked() -> Self {
        Self {
            handle: std::ptr::null_mut(),
            held: false,
        }
    }
}

/// The SDDL of a mutex only `user_sid` may use: owned by that SID, full access
/// for it, and a medium integrity label that no lower-integrity process may
/// read, write or execute up to. The explicit owner keeps the mutex of an
/// elevated instance owned by the user rather than by Administrators.
fn mutex_sddl(user_sid: &str) -> String {
    format!("O:{user_sid}D:(A;;GA;;;{user_sid})S:(ML;;NWNRNX;;;ME)")
}

/// The string form of the current user's SID (`S-1-5-21-...`).
fn current_user_sid() -> Option<String> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: `token` is a valid out-pointer for the call.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return None;
    }
    let sid = (|| {
        let mut needed = 0u32;
        // SAFETY: a size query: no buffer, `needed` is a valid out-pointer.
        unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
        if needed == 0 {
            return None;
        }
        // u64 elements keep the buffer aligned for TOKEN_USER.
        let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
        // SAFETY: the buffer is at least `needed` bytes.
        let filled = unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        };
        if filled == 0 {
            return None;
        }
        // SAFETY: the call filled the buffer with a TOKEN_USER.
        let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
        let mut text: *mut u16 = std::ptr::null_mut();
        // SAFETY: `user.User.Sid` points into `buffer`, which is alive.
        if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) } == 0 {
            return None;
        }
        // SAFETY: on success `text` is a NUL-terminated string that this
        // reads and then frees with `LocalFree`, as the API requires.
        unsafe {
            let length = (0..).take_while(|&i| *text.add(i) != 0).count();
            let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, length));
            LocalFree(text.cast());
            Some(sid)
        }
    })();
    // SAFETY: `token` was opened above and is not used after this.
    unsafe { CloseHandle(token) };
    sid
}

/// The current user's SID and the SDDL of the mutexes this process creates,
/// `None` if the SID can't be read.
fn own_sddl() -> Option<&'static (String, String)> {
    static SDDL: std::sync::OnceLock<Option<(String, String)>> = std::sync::OnceLock::new();
    SDDL.get_or_init(|| current_user_sid().map(|sid| (mutex_sddl(&sid), sid)))
        .as_ref()
}

/// Creates (or opens) the mutex `wide_name` with the security descriptor
/// `sddl`. Returns the handle (null on failure) and whether the name already
/// existed, in which case `sddl` was not applied.
fn create_mutex(wide_name: &[u16], sddl: &str) -> (HANDLE, bool) {
    let wide_sddl = crate::wide::encode_wide(sddl);
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `wide_sddl` is a NUL-terminated UTF-16 string; on success the
    // descriptor is freed below.
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide_sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    };
    if converted == 0 {
        return (std::ptr::null_mut(), false);
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    // SAFETY: `attributes` and `wide_name` (NUL-terminated) outlive the call;
    // the error is reset first so the read after it is this call's.
    let (handle, existed) = unsafe {
        SetLastError(0);
        let handle = CreateMutexW(&attributes, 0, wide_name.as_ptr());
        (handle, GetLastError() == ERROR_ALREADY_EXISTS)
    };
    // SAFETY: allocated by the conversion above, not used after this.
    unsafe { LocalFree(descriptor) };
    (handle, existed)
}

/// The integrity level of the object `handle`, from its mandatory label
/// (`MEDIUM_RID` when it has none); `None` if it can't be read.
fn integrity_rid(handle: HANDLE) -> Option<u32> {
    let mut needed = 0u32;
    // SAFETY: a size query: no buffer, `needed` is a valid out-pointer.
    unsafe {
        GetKernelObjectSecurity(
            handle,
            LABEL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            0,
            &mut needed,
        )
    };
    if needed == 0 {
        return None;
    }
    // u64 elements keep the buffer aligned for the descriptor and its ACL.
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
    let descriptor: PSECURITY_DESCRIPTOR = buffer.as_mut_ptr().cast();
    // SAFETY: the buffer is at least `needed` bytes.
    let read = unsafe {
        GetKernelObjectSecurity(
            handle,
            LABEL_SECURITY_INFORMATION,
            descriptor,
            needed,
            &mut needed,
        )
    };
    if read == 0 {
        return None;
    }
    let mut present = 0;
    let mut defaulted = 0;
    let mut sacl = std::ptr::null_mut();
    // SAFETY: `descriptor` is the valid descriptor just read; the out-pointers
    // are valid.
    let ok =
        unsafe { GetSecurityDescriptorSacl(descriptor, &mut present, &mut sacl, &mut defaulted) };
    if ok == 0 {
        return None;
    }
    let mut ace = std::ptr::null_mut();
    // SAFETY: `sacl` points into `buffer` when present; GetAce checks the index.
    if present == 0 || sacl.is_null() || unsafe { GetAce(sacl, 0, &mut ace) } == 0 {
        return Some(MEDIUM_RID);
    }
    let ace = ace.cast::<SYSTEM_MANDATORY_LABEL_ACE>();
    // SAFETY: `ace` points at an ACE inside `buffer`; its SID follows the mask.
    unsafe {
        if (*ace).Header.AceType != MANDATORY_LABEL_ACE_TYPE {
            return Some(MEDIUM_RID);
        }
        let sid = std::ptr::addr_of_mut!((*ace).SidStart).cast();
        let count = *GetSidSubAuthorityCount(sid);
        (count > 0).then(|| *GetSidSubAuthority(sid, u32::from(count) - 1))
    }
}

/// The string form of the owner SID of the object `handle`; `None` if it
/// can't be read.
fn owner_sid(handle: HANDLE) -> Option<String> {
    let mut needed = 0u32;
    // SAFETY: a size query: no buffer, `needed` is a valid out-pointer.
    unsafe {
        GetKernelObjectSecurity(
            handle,
            OWNER_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            0,
            &mut needed,
        )
    };
    if needed == 0 {
        return None;
    }
    // u64 elements keep the buffer aligned for the descriptor.
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
    let descriptor: PSECURITY_DESCRIPTOR = buffer.as_mut_ptr().cast();
    // SAFETY: the buffer is at least `needed` bytes.
    let read = unsafe {
        GetKernelObjectSecurity(
            handle,
            OWNER_SECURITY_INFORMATION,
            descriptor,
            needed,
            &mut needed,
        )
    };
    if read == 0 {
        return None;
    }
    let mut owner = std::ptr::null_mut();
    let mut defaulted = 0;
    // SAFETY: `descriptor` is the valid descriptor just read; the out-pointers
    // are valid.
    if unsafe { GetSecurityDescriptorOwner(descriptor, &mut owner, &mut defaulted) } == 0
        || owner.is_null()
    {
        return None;
    }
    let mut text: *mut u16 = std::ptr::null_mut();
    // SAFETY: `owner` points into `buffer`, which is alive.
    if unsafe { ConvertSidToStringSidW(owner, &mut text) } == 0 {
        return None;
    }
    // SAFETY: on success `text` is a NUL-terminated string that this reads and
    // then frees with `LocalFree`, as the API requires.
    unsafe {
        let length = (0..).take_while(|&i| *text.add(i) != 0).count();
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, length));
        LocalFree(text.cast());
        Some(sid)
    }
}

/// Takes the mutex `name` (`Local\...`), waiting up to `timeout`. `None` if
/// another thread or process still holds it after that. The same thread can
/// take it again while it holds it. A mutex that can't be created or trusted
/// gives a guard that holds nothing (see the module docs).
pub fn lock(name: &str, timeout: Duration) -> Option<NamedMutexGuard> {
    let Some((sddl, sid)) = own_sddl() else {
        log!("couldn't read the user's SID; continuing without the mutex {name}");
        return Some(NamedMutexGuard::unlocked());
    };
    lock_with(name, sddl, Some(sid), timeout)
}

/// [`lock`] creating the mutex with `sddl`. An existing mutex is trusted only
/// when it carries at least a medium label and, with `owner` given, is owned
/// by that SID.
fn lock_with(
    name: &str,
    sddl: &str,
    owner: Option<&str>,
    timeout: Duration,
) -> Option<NamedMutexGuard> {
    let wide = crate::wide::encode_wide(name);
    let (handle, existed) = create_mutex(&wide, sddl);
    if handle.is_null() {
        log!("couldn't create the mutex {name}; continuing without it");
        return Some(NamedMutexGuard::unlocked());
    }
    if existed && integrity_rid(handle).is_none_or(|rid| rid < MEDIUM_RID) {
        log!(
            "the mutex {name} was created by a lower-integrity process; ignoring it and continuing without it"
        );
        // SAFETY: `handle` is valid and owned by this function.
        unsafe { CloseHandle(handle) };
        return Some(NamedMutexGuard::unlocked());
    }
    if existed
        && let Some(owner) = owner
        && owner_sid(handle).as_deref() != Some(owner)
    {
        log!("the mutex {name} is owned by another user; ignoring it and continuing without it");
        // SAFETY: `handle` is valid and owned by this function.
        unsafe { CloseHandle(handle) };
        return Some(NamedMutexGuard::unlocked());
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
    fn a_mutex_it_creates_is_limited_to_the_user_and_labelled_medium() {
        let sid = current_user_sid().expect("the user's SID");
        assert!(sid.starts_with("S-1-"), "{sid}");
        assert!(mutex_sddl(&sid).contains(&format!("(A;;GA;;;{sid})")));

        let name = unique_name("label");
        let guard = lock(&name, Duration::ZERO).expect("free");
        assert_eq!(integrity_rid(guard.handle), Some(MEDIUM_RID));
        assert_eq!(owner_sid(guard.handle), Some(sid));
    }

    #[test]
    fn a_name_owned_by_another_user_is_ignored() {
        let sid = current_user_sid().expect("the user's SID");
        let name = unique_name("owner");
        let squat = lock(&name, Duration::ZERO).expect("free");

        // Same object, but it must be owned by "someone else" (LocalSystem).
        let held = std::thread::spawn({
            let (name, sddl) = (name.clone(), mutex_sddl(&sid));
            move || {
                lock_with(&name, &sddl, Some("S-1-5-18"), Duration::from_millis(30))
                    .is_some_and(|g| !g.held)
            }
        });

        assert!(held.join().unwrap(), "ignored, not waited on");
        drop(squat);
    }

    #[test]
    fn a_name_pre_created_and_held_by_a_lower_integrity_process_does_not_block() {
        const LOW_INTEGRITY: &str = "D:(A;;GA;;;WD)S:(ML;;NW;;;LW)";
        let name = unique_name("squatted");
        let (held_tx, held) = std::sync::mpsc::channel();
        let (release, hold) = std::sync::mpsc::channel::<()>();
        let squatter = {
            let name = name.clone();
            std::thread::spawn(move || {
                // What a sandboxed process could do: create the name first, hold it.
                let squat = lock_with(&name, LOW_INTEGRITY, None, Duration::ZERO).expect("free");
                held_tx.send(()).unwrap();
                let _ = hold.recv();
                drop(squat);
            })
        };
        held.recv().unwrap();

        let ours = lock(&name, Duration::from_millis(30));

        release.send(()).unwrap();
        squatter.join().unwrap();
        let ours = ours.expect("not blocked by the squatter");
        assert!(!ours.held, "it proceeds without the mutex it can't trust");
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
