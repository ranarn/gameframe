//! Single-instance guard: a second `gameframe.exe` exits immediately instead
//! of running alongside the first (two tray icons, two keyboard hooks, two
//! WinEvent hooks fighting over the same windows).
//!
//! Uses a named mutex in the session's `Local\` namespace, so each logged-in
//! user gets their own instance. The winning handle is deliberately never
//! closed -- the OS releases it when the process exits, which is exactly the
//! lifetime the guard needs. Handles from lost attempts are closed at once so
//! retrying doesn't leak one handle per attempt.
//!
//! An elevated relaunch (see `win32::relaunch_elevated`) starts the new
//! process before the old one has exited, so a process started with
//! [`ELEVATED_RELAUNCH_FLAG`] polls for the mutex for a few seconds instead
//! of giving up on the first attempt.

use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, GetLastError,
};
use windows_sys::Win32::System::Threading::CreateMutexW;

/// Same GUID as the installer's `AppId`, so the name can't collide with
/// another app's mutex.
pub(crate) const MUTEX_NAME: &str = "Local\\GameFrame-5C28E192-0AB5-4FA5-8874-88144D010E3B";

/// Command-line flag passed to the elevated child by `relaunch_elevated`.
pub(crate) const ELEVATED_RELAUNCH_FLAG: &str = "--elevated-relaunch";

/// Command-line flag that asks the running instance to quit, see
/// [`is_quit_request`].
pub const QUIT_FLAG: &str = "--quit";

/// How long an elevated relaunch waits for the old instance to exit.
pub(crate) const RELAUNCH_WAIT: Duration = Duration::from_secs(5);

/// Polling interval while waiting for the old instance to release the mutex.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// What to do after one attempt at taking the mutex.
#[derive(Debug, PartialEq, Eq)]
enum Decision {
    /// This process owns the instance; start up.
    Proceed,
    /// Another instance still holds it; try again after a short sleep.
    Retry,
    /// Another instance holds it and we are not (or no longer) waiting.
    GiveUp,
}

/// Pure retry decision. `owned` is the attempt result, `elapsed` the time
/// since the first attempt, `wait` the allowed total (`None` = single attempt,
/// the normal launch).
fn decide(owned: bool, elapsed: Duration, wait: Option<Duration>) -> Decision {
    if owned {
        return Decision::Proceed;
    }
    match wait {
        Some(limit) if elapsed < limit => Decision::Retry,
        _ => Decision::GiveUp,
    }
}

/// Whether the process arguments (program name first) carry the elevated
/// relaunch flag.
pub fn is_elevated_relaunch<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter()
        .skip(1)
        .any(|a| a.as_ref() == ELEVATED_RELAUNCH_FLAG)
}

/// Whether the process arguments (program name first) are a request to quit
/// the running instance (`gameframe.exe --quit`): the process sends `Quit` and
/// exits without taking the instance. Used by hand and by the updater.
pub fn is_quit_request<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter().skip(1).any(|a| a.as_ref() == QUIT_FLAG)
}

/// An unusual `CreateMutexW` outcome worth a log line. `acquire` hands the
/// first one back to `main` instead of logging it, because the log may not be
/// open yet when the mutex is taken; `main` opens the log only when there is
/// a note (or later, as usual), then writes it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Note {
    /// An existing mutex can't be opened; treated as held.
    AccessDenied,
    /// The call failed for another reason; treated as not held (fail open).
    CreateFailed(u32),
}

impl std::fmt::Display for Note {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Note::AccessDenied => f.write_str(
                "single-instance mutex exists but cannot be opened (access denied); treating it as held by another instance",
            ),
            Note::CreateFailed(code) => write!(
                f,
                "single-instance mutex could not be created (error {code}); starting anyway"
            ),
        }
    }
}

/// Returns whether this process is the only running GameFrame (`false` if
/// another instance holds the mutex), plus the first unusual outcome seen, for
/// the caller to log. With `wait`, keeps polling for up to that long (an
/// elevated relaunch waiting for the old process to exit); a timeout is
/// logged. If the mutex can't be created at all, returns `true` -- failing
/// open beats refusing to start.
pub fn acquire(wait: Option<Duration>) -> (bool, Option<Note>) {
    let start = Instant::now();
    let mut note = None;
    loop {
        let (owned, attempt_note) = try_acquire(MUTEX_NAME);
        // Keep only the first: an elevated relaunch polls for seconds.
        note = note.or(attempt_note);
        match decide(owned, start.elapsed(), wait) {
            Decision::Proceed => return (true, note),
            Decision::Retry => std::thread::sleep(POLL_INTERVAL),
            Decision::GiveUp => {
                if let Some(limit) = wait {
                    log!(
                        "elevated relaunch: previous instance still held the single-instance mutex after {limit:?}; exiting"
                    );
                }
                return (false, note);
            }
        }
    }
}

/// One attempt. On success the handle is intentionally kept open for the
/// process lifetime; on `ERROR_ALREADY_EXISTS` it is closed.
fn try_acquire(mutex_name: &str) -> (bool, Option<Note>) {
    let name: Vec<u16> = mutex_name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `name` is a NUL-terminated UTF-16 string that outlives the
    // call; null security attributes are valid. GetLastError is read
    // immediately after, before any other API call can overwrite it.
    let (handle, last_error) = unsafe {
        let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        (handle, GetLastError())
    };
    let failed = handle.is_null();
    let held = mutex_held(failed, last_error);
    if held && !failed {
        // SAFETY: `handle` is a valid mutex handle we just received and
        // haven't closed; this drops our reference to the existing mutex.
        unsafe { CloseHandle(handle) };
    }
    (!held, note_for(failed, last_error))
}

/// Pure mapping of a `CreateMutexW` result to "another instance holds the
/// mutex". `handle_is_null` is whether the call failed; `last_error` is
/// `GetLastError` right after it. `ERROR_ACCESS_DENIED` (an existing mutex we
/// may not open, e.g. the other instance runs elevated with other
/// credentials) means held; any other failure is not held (fail open).
fn mutex_held(handle_is_null: bool, last_error: u32) -> bool {
    if handle_is_null {
        last_error == ERROR_ACCESS_DENIED
    } else {
        last_error == ERROR_ALREADY_EXISTS
    }
}

/// Pure: the diagnostic for a failed `CreateMutexW` (`None` when it worked).
fn note_for(handle_is_null: bool, last_error: u32) -> Option<Note> {
    match (handle_is_null, last_error) {
        (false, _) => None,
        (true, ERROR_ACCESS_DENIED) => Some(Note::AccessDenied),
        (true, code) => Some(Note::CreateFailed(code)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WAIT: Option<Duration> = Some(Duration::from_secs(5));

    #[test]
    fn owning_the_mutex_proceeds_regardless_of_wait() {
        assert_eq!(decide(true, Duration::ZERO, None), Decision::Proceed);
        assert_eq!(
            decide(true, Duration::from_secs(9), WAIT),
            Decision::Proceed
        );
    }

    #[test]
    fn normal_launch_gives_up_on_first_failed_attempt() {
        assert_eq!(decide(false, Duration::ZERO, None), Decision::GiveUp);
    }

    #[test]
    fn elevated_relaunch_retries_until_timeout() {
        assert_eq!(decide(false, Duration::ZERO, WAIT), Decision::Retry);
        assert_eq!(
            decide(false, Duration::from_millis(4999), WAIT),
            Decision::Retry
        );
        assert_eq!(
            decide(false, Duration::from_secs(5), WAIT),
            Decision::GiveUp
        );
        assert_eq!(
            decide(false, Duration::from_secs(6), WAIT),
            Decision::GiveUp
        );
    }

    #[test]
    fn mutex_held_mapping() {
        assert!(mutex_held(true, ERROR_ACCESS_DENIED));
        assert!(mutex_held(false, ERROR_ALREADY_EXISTS));
        assert!(!mutex_held(false, 0));
        assert!(!mutex_held(true, 8)); // other failure: fail open
    }

    #[test]
    fn a_held_mutex_makes_further_attempts_fail() {
        // Own name, so a running GameFrame can't interfere.
        let name = format!("Local\\GameFrame-test-{}", std::process::id());
        assert!(try_acquire(&name).0);
        for _ in 0..3 {
            assert!(!try_acquire(&name).0);
        }
    }

    #[test]
    fn failed_calls_produce_a_note_and_successful_ones_none() {
        assert_eq!(
            note_for(true, ERROR_ACCESS_DENIED),
            Some(Note::AccessDenied)
        );
        assert_eq!(note_for(true, 8), Some(Note::CreateFailed(8)));
        assert_eq!(note_for(false, ERROR_ALREADY_EXISTS), None);
        assert_eq!(note_for(false, 0), None);
        assert!(Note::CreateFailed(8).to_string().contains("error 8"));
    }

    #[test]
    fn quit_flag_is_detected_after_program_name_only() {
        assert!(is_quit_request(["gameframe.exe", "--quit"]));
        assert!(!is_quit_request(["gameframe.exe"]));
        assert!(!is_quit_request(["--quit"]));
        assert!(!is_quit_request(["gameframe.exe", "--elevated-relaunch"]));
    }

    #[test]
    fn flag_is_detected_after_program_name_only() {
        assert!(is_elevated_relaunch([
            "gameframe.exe",
            "--elevated-relaunch"
        ]));
        assert!(!is_elevated_relaunch(["gameframe.exe"]));
        assert!(!is_elevated_relaunch(["--elevated-relaunch"]));
        assert!(!is_elevated_relaunch(["gameframe.exe", "--other"]));
    }
}
