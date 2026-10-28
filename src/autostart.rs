//! "Start with Windows" -- registers/unregisters GameFrame under the
//! per-user `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` key.
//!
//! The registry is the single source of truth: nothing about autostart is
//! stored in `config.toml`, because the installer, the tray menu, the
//! Settings window and Task Manager can all change it independently. Every
//! caller re-reads [`is_enabled`] rather than caching the answer.
//!
//! Task Manager's "Disable" doesn't delete the `Run` value -- it writes an
//! override under `Explorer\StartupApproved\Run`. That override's binary
//! format is undocumented, so [`is_enabled`] doesn't try to interpret it;
//! instead, enabling deletes any override for our value name, which reliably
//! restores the default (enabled) state. A Task-Manager-disabled entry
//! therefore still reads as enabled here until toggled.

use crate::win32::encode_wide;
use std::path::Path;

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, REG_SZ, RRF_RT_ANY, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};

/// The `Run` value name. The installer (`installer/gameframe.iss`) writes and
/// removes the same name, so the two must stay in sync.
const VALUE_NAME: &str = "GameFrame";
const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const STARTUP_APPROVED_SUBKEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

#[derive(Debug)]
pub enum AutostartError {
    CurrentExe(std::io::Error),
    Registry {
        operation: &'static str,
        code: WIN32_ERROR,
    },
}

impl std::fmt::Display for AutostartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AutostartError::CurrentExe(e) => {
                write!(f, "could not resolve GameFrame's executable path: {e}")
            }
            AutostartError::Registry { operation, code } => {
                write!(f, "registry {operation} failed with Win32 error {code}")
            }
        }
    }
}

impl std::error::Error for AutostartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AutostartError::CurrentExe(e) => Some(e),
            AutostartError::Registry { .. } => None,
        }
    }
}

/// Where the autostart entry lives. Production always uses
/// [`RegistryLocation::windows_run`]; tests point it at a throwaway key so
/// they never touch the user's real `Run` key.
struct RegistryLocation<'a> {
    root: HKEY,
    run_subkey: &'a str,
    startup_approved_subkey: &'a str,
    value_name: &'a str,
}

impl RegistryLocation<'static> {
    fn windows_run() -> Self {
        Self {
            root: HKEY_CURRENT_USER,
            run_subkey: RUN_SUBKEY,
            startup_approved_subkey: STARTUP_APPROVED_SUBKEY,
            value_name: VALUE_NAME,
        }
    }
}

/// Whether GameFrame is registered to start with Windows.
pub fn is_enabled() -> bool {
    is_enabled_at(&RegistryLocation::windows_run())
}

/// Registers (pointing at the currently running executable) or unregisters
/// GameFrame from starting with Windows.
pub fn set_enabled(enabled: bool) -> Result<(), AutostartError> {
    let location = RegistryLocation::windows_run();
    if enabled {
        let exe = std::env::current_exe().map_err(AutostartError::CurrentExe)?;
        enable_at(&location, &exe)
    } else {
        disable_at(&location)
    }
}

/// The `Run` value's command line for `exe`: the path in double quotes, so a
/// path with spaces (e.g. under `C:\Users\First Last\...`) isn't split by the
/// shell into an executable plus arguments.
fn run_command(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

fn is_enabled_at(location: &RegistryLocation) -> bool {
    let subkey = encode_wide(location.run_subkey);
    let value = encode_wide(location.value_name);
    let mut size: u32 = 0;
    // A size-only query: succeeds iff the value exists, whatever its type.
    // SAFETY: the key and value names are NUL-terminated UTF-16 strings and the data/size arguments are valid for the stated length; all outlive the call.
    let code = unsafe {
        RegGetValueW(
            location.root,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_ANY,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    code == ERROR_SUCCESS
}

fn enable_at(location: &RegistryLocation, exe: &Path) -> Result<(), AutostartError> {
    let data = encode_wide(&run_command(exe));
    let subkey = encode_wide(location.run_subkey);
    let value = encode_wide(location.value_name);
    // SAFETY: the key and value names are NUL-terminated UTF-16 strings and the data/size arguments are valid for the stated length; all outlive the call.
    let code = unsafe {
        RegSetKeyValueW(
            location.root,
            subkey.as_ptr(),
            value.as_ptr(),
            REG_SZ,
            data.as_ptr().cast(),
            (data.len() * size_of::<u16>()) as u32,
        )
    };
    if code != ERROR_SUCCESS {
        return Err(AutostartError::Registry {
            operation: "write",
            code,
        });
    }
    // Clear a Task Manager "Disabled" override so the entry actually runs.
    delete_value(
        location.root,
        location.startup_approved_subkey,
        location.value_name,
    )
}

fn disable_at(location: &RegistryLocation) -> Result<(), AutostartError> {
    delete_value(location.root, location.run_subkey, location.value_name)?;
    delete_value(
        location.root,
        location.startup_approved_subkey,
        location.value_name,
    )
}

/// Deletes one value; a value (or key) that doesn't exist counts as success.
fn delete_value(root: HKEY, subkey: &str, value_name: &str) -> Result<(), AutostartError> {
    let subkey = encode_wide(subkey);
    let value = encode_wide(value_name);
    // SAFETY: `subkey` and `value` are NUL-terminated UTF-16 strings that outlive the call.
    let code = unsafe { RegDeleteKeyValueW(root, subkey.as_ptr(), value.as_ptr()) };
    match code {
        ERROR_SUCCESS | ERROR_FILE_NOT_FOUND => Ok(()),
        code => Err(AutostartError::Registry {
            operation: "delete",
            code,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Registry::{RRF_RT_REG_SZ, RegDeleteTreeW};

    /// A throwaway `HKCU\Software\GameFrameTest-<pid>-<name>` key, deleted on
    /// drop, standing in for both the `Run` and `StartupApproved\Run` keys.
    struct ScratchKey {
        run: String,
        approved: String,
        root_key: String,
    }

    impl ScratchKey {
        fn new(name: &str) -> Self {
            let root_key = format!(r"Software\GameFrameTest-{}-{name}", std::process::id());
            Self {
                run: format!(r"{root_key}\Run"),
                approved: format!(r"{root_key}\Approved"),
                root_key,
            }
        }

        fn location(&self) -> RegistryLocation<'_> {
            RegistryLocation {
                root: HKEY_CURRENT_USER,
                run_subkey: &self.run,
                startup_approved_subkey: &self.approved,
                value_name: VALUE_NAME,
            }
        }

        fn read_run_value(&self) -> Option<String> {
            let subkey = encode_wide(&self.run);
            let value = encode_wide(VALUE_NAME);
            let mut buf = [0u16; 1024];
            let mut size = (buf.len() * size_of::<u16>()) as u32;
            // SAFETY: the key and value names are NUL-terminated UTF-16 strings and the data/size arguments are valid for the stated length; all outlive the call.
            let code = unsafe {
                RegGetValueW(
                    HKEY_CURRENT_USER,
                    subkey.as_ptr(),
                    value.as_ptr(),
                    RRF_RT_REG_SZ,
                    std::ptr::null_mut(),
                    buf.as_mut_ptr().cast(),
                    &mut size,
                )
            };
            (code == ERROR_SUCCESS).then(|| {
                let len = size as usize / size_of::<u16>();
                String::from_utf16_lossy(&buf[..len])
                    .trim_end_matches('\0')
                    .to_owned()
            })
        }

        fn set_approved_override(&self) {
            let subkey = encode_wide(&self.approved);
            let value = encode_wide(VALUE_NAME);
            let data = [3u8; 12];
            // SAFETY: the key and value names are NUL-terminated UTF-16 strings and the data/size arguments are valid for the stated length; all outlive the call.
            let code = unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    subkey.as_ptr(),
                    value.as_ptr(),
                    windows_sys::Win32::System::Registry::REG_BINARY,
                    data.as_ptr().cast(),
                    data.len() as u32,
                )
            };
            assert_eq!(code, ERROR_SUCCESS);
        }

        fn approved_override_exists(&self) -> bool {
            let subkey = encode_wide(&self.approved);
            let value = encode_wide(VALUE_NAME);
            let mut size = 0u32;
            // SAFETY: the key and value names are NUL-terminated UTF-16 strings and the data/size arguments are valid for the stated length; all outlive the call.
            let code = unsafe {
                RegGetValueW(
                    HKEY_CURRENT_USER,
                    subkey.as_ptr(),
                    value.as_ptr(),
                    RRF_RT_ANY,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut size,
                )
            };
            code == ERROR_SUCCESS
        }
    }

    impl Drop for ScratchKey {
        fn drop(&mut self) {
            let key = encode_wide(&self.root_key);
            // Given a subkey, RegDeleteTreeW removes that key itself along with
            // everything under it.
            // SAFETY: `key` is a NUL-terminated UTF-16 string that outlives the call.
            unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, key.as_ptr()) };
        }
    }

    #[test]
    fn run_command_quotes_a_path_containing_spaces() {
        let command = run_command(Path::new(
            r"C:\Users\First Last\AppData\Local\Programs\GameFrame\gameframe.exe",
        ));

        assert_eq!(
            command,
            r#""C:\Users\First Last\AppData\Local\Programs\GameFrame\gameframe.exe""#
        );
    }

    #[test]
    fn is_enabled_is_false_when_no_value_was_ever_written() {
        let scratch = ScratchKey::new("never-written");

        assert!(!is_enabled_at(&scratch.location()));
    }

    #[test]
    fn enabling_writes_the_quoted_exe_path_and_reads_back_as_enabled() {
        let scratch = ScratchKey::new("enable");
        let exe = Path::new(r"C:\Program Files\GameFrame\gameframe.exe");

        enable_at(&scratch.location(), exe).unwrap();

        assert!(is_enabled_at(&scratch.location()));
        assert_eq!(
            scratch.read_run_value().as_deref(),
            Some(r#""C:\Program Files\GameFrame\gameframe.exe""#)
        );
    }

    #[test]
    fn enabling_clears_a_task_manager_disabled_override() {
        let scratch = ScratchKey::new("clear-override");
        scratch.set_approved_override();

        enable_at(&scratch.location(), Path::new(r"C:\gameframe.exe")).unwrap();

        assert!(!scratch.approved_override_exists());
    }

    #[test]
    fn disabling_removes_the_value_and_reads_back_as_disabled() {
        let scratch = ScratchKey::new("disable");
        enable_at(&scratch.location(), Path::new(r"C:\gameframe.exe")).unwrap();

        disable_at(&scratch.location()).unwrap();

        assert!(!is_enabled_at(&scratch.location()));
    }

    #[test]
    fn disabling_when_already_disabled_succeeds() {
        let scratch = ScratchKey::new("disable-twice");

        disable_at(&scratch.location()).unwrap();
        disable_at(&scratch.location()).unwrap();

        assert!(!is_enabled_at(&scratch.location()));
    }
}
