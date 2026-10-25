//! Update install: downloads the installer of an available update into a
//! fresh temp folder, checks it against the release's SHA-256 (a file that
//! fails is deleted and never run) and starts it silently and detached.
//!
//! The installer asks the running instance to quit before it replaces files
//! (the same command as tray Exit) and, because of its `/RELAUNCH` parameter,
//! starts GameFrame again afterwards. It cannot delete itself, so
//! [`clean_stale_downloads`] removes its folder at the next start.
//!
//! The verified file is held open from the hash to the start of the process,
//! with a share mode that denies writing and deleting, so nothing can swap it
//! in between (that matters when GameFrame runs elevated). The download and the
//! process start are OS-facing; the verification, the file handling and the
//! cleanup are unit tested.

use crate::http::HttpError;
use crate::updater::{
    ASSET_HOST, FlagGuard, SpawnError, UpdateError, UpdateInfo, installer_name, verify_sha256,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// The host `github.com` redirects a release-asset download to (a 302 whose
/// `Location` carries a signed URL).
const ASSET_REDIRECT_HOST: &str = "release-assets.githubusercontent.com";

/// Largest installer a download accepts; the real one is a few MB.
const MAX_INSTALLER_BYTES: u64 = 100 * 1024 * 1024;

/// Name prefix of the temp folder an install downloads into.
const UPDATE_DIR_PREFIX: &str = "GameFrame-update-";

/// The installer's command line: silent, no restart, and the custom switch
/// that makes it start GameFrame once it is done (`RelaunchRequested` in
/// `installer/gameframe.iss`).
const INSTALLER_ARGS: [&str; 4] = [
    "/VERYSILENT",
    "/SUPPRESSMSGBOXES",
    "/NORESTART",
    "/RELAUNCH",
];

/// Whether an install started by [`spawn_install`] is still running.
static INSTALL_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// The text shown to the user for a failed install.
pub(crate) fn install_error_message(error: &UpdateError) -> String {
    format!("Installing the update failed: {error}")
}

/// A downloaded installer whose digest matched. `_lock` is the handle the
/// digest was computed through, opened with a share mode that denies write
/// and delete: while it is open, the file cannot change.
struct VerifiedInstaller {
    path: PathBuf,
    _lock: std::fs::File,
}

/// Writes the installer into `dir` through `download`, then verifies it
/// against the digest of `info`. Any failure deletes `dir`, so a file that was
/// not verified is never left to be run.
fn prepare_installer(
    info: &UpdateInfo,
    dir: &Path,
    download: impl FnOnce(&mut std::fs::File) -> Result<(), UpdateError>,
) -> Result<VerifiedInstaller, UpdateError> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 0x1;
    let path = dir.join(installer_name(info.version));
    let result = (|| {
        let io = |e: std::io::Error| UpdateError::Download(e.to_string());
        let mut file = std::fs::File::create_new(&path).map_err(io)?;
        download(&mut file)?;
        drop(file);
        // Hash through the handle that is kept, not through a second open:
        // whatever was written, this is the content that gets run.
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&path)
            .map_err(io)?;
        verify_sha256(&lock, &info.sha256)?;
        Ok(lock)
    })();
    match result {
        Ok(lock) => Ok(VerifiedInstaller { path, _lock: lock }),
        Err(e) => {
            let _ = std::fs::remove_dir_all(dir);
            Err(e)
        }
    }
}

/// Creates an empty, uniquely named update folder in the temp directory.
fn fresh_update_dir() -> std::io::Result<PathBuf> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir =
        std::env::temp_dir().join(format!("{UPDATE_DIR_PREFIX}{}-{nanos}", std::process::id()));
    std::fs::create_dir(&dir)?;
    Ok(dir)
}

/// Starts the verified installer silently, as a detached process that outlives
/// GameFrame. The file stays locked until the process has been created.
fn launch_installer(installer: VerifiedInstaller) -> Result<(), UpdateError> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    std::process::Command::new(&installer.path)
        .args(INSTALLER_ARGS)
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .map(drop)
        .map_err(|e| UpdateError::LaunchInstaller(e.to_string()))
}

/// Downloads, verifies and starts the installer of `info`. On any failure the
/// download is deleted and nothing has been run.
fn install(info: &UpdateInfo) -> Result<(), UpdateError> {
    let dir = fresh_update_dir().map_err(|e| UpdateError::Download(e.to_string()))?;
    let installer = prepare_installer(info, &dir, |file| {
        crate::http::download(
            &info.download_url,
            &[ASSET_HOST, ASSET_REDIRECT_HOST],
            file,
            MAX_INSTALLER_BYTES,
        )
        .map_err(|e| match e {
            HttpError::Status(status) => UpdateError::Download(format!(
                "the download server answered with HTTP status {status}"
            )),
            HttpError::Failed(e) => UpdateError::Download(e),
        })
    })?;
    launch_installer(installer).inspect_err(|_| {
        let _ = std::fs::remove_dir_all(&dir);
    })
}

/// Installs the update `info` on a worker thread: `on_start` runs there first,
/// then download, verify, start the installer, then `on_done` gets the result.
/// After a failure the in-flight flag is already released when `on_done` runs.
/// After a success `on_done` runs with it still held, and it is released only
/// when `on_done` returns, so `on_done` can wait for the installer to quit this
/// instance (which then starts it again) without a second install starting
/// meanwhile. If the installer fails, this instance keeps running. Starts
/// nothing while an earlier install is still running.
pub(crate) fn spawn_install(
    info: UpdateInfo,
    on_start: impl FnOnce() + Send + 'static,
    on_done: impl FnOnce(Result<(), UpdateError>) + Send + 'static,
) -> Result<(), SpawnError> {
    let Some(in_flight) = FlagGuard::acquire(&INSTALL_IN_FLIGHT) else {
        return Err(SpawnError::Busy);
    };
    std::thread::Builder::new()
        .name("update-install".into())
        .spawn(move || {
            on_start();
            match install(&info) {
                Ok(()) => {
                    on_done(Ok(()));
                    drop(in_flight);
                }
                Err(e) => {
                    log!("update install failed: {e}");
                    drop(in_flight);
                    on_done(Err(e));
                }
            }
        })
        .map(drop)
        .map_err(|e| {
            log!("update install: couldn't start the worker thread: {e}");
            SpawnError::ThreadFailed
        })
}

/// Deletes the update folders in `temp` except those of the process `own_pid`,
/// whose install may be writing to its folder right now, and returns how many could not be
/// deleted (a running installer holds its file open).
fn remove_stale_update_dirs(temp: &Path, own_pid: u32) -> usize {
    let own_prefix = format!("{UPDATE_DIR_PREFIX}{own_pid}-");
    let Ok(entries) = std::fs::read_dir(temp) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(UPDATE_DIR_PREFIX)
                && !entry.file_name().to_string_lossy().starts_with(&own_prefix)
                && entry.path().is_dir()
        })
        .filter(|entry| std::fs::remove_dir_all(entry.path()).is_err())
        .count()
}

/// Removes the folders earlier installs downloaded into. Runs on a worker
/// thread and retries for a while, because right after an update the installer
/// that relaunched GameFrame is still exiting.
pub(crate) fn clean_stale_downloads() {
    const ATTEMPTS: u32 = 10;
    const RETRY_AFTER: Duration = Duration::from_secs(2);
    let spawned = std::thread::Builder::new()
        .name("update-cleanup".into())
        .spawn(|| {
            let temp = std::env::temp_dir();
            for _ in 0..ATTEMPTS {
                if remove_stale_update_dirs(&temp, std::process::id()) == 0 {
                    return;
                }
                std::thread::sleep(RETRY_AFTER);
            }
            log!("update cleanup: some old update downloads could not be deleted");
        });
    if let Err(e) = spawned {
        log!("update cleanup: couldn't start the worker thread: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::updater::Version;
    use sha2::Digest;

    const SHA_ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn info_for(content_digest: &str) -> UpdateInfo {
        UpdateInfo {
            version: Version::parse("1.2.0").unwrap(),
            download_url: "https://github.com/a.exe".into(),
            sha256: content_digest.into(),
        }
    }

    fn write_abc(file: &mut std::fs::File) -> Result<(), UpdateError> {
        use std::io::Write;
        file.write_all(b"abc")
            .map_err(|e| UpdateError::Download(e.to_string()))
    }

    fn update_dir(temp: &tempfile::TempDir) -> PathBuf {
        let dir = temp.path().join("GameFrame-update-1");
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    #[test]
    fn prepare_installer_returns_the_verified_file() {
        let temp = tempfile::tempdir().unwrap();
        let dir = update_dir(&temp);

        let installer = prepare_installer(&info_for(SHA_ABC), &dir, write_abc).unwrap();

        assert_eq!(installer.path, dir.join("GameFrame-Setup-1.2.0.exe"));
        assert_eq!(std::fs::read(&installer.path).unwrap(), b"abc");
    }

    #[test]
    fn prepare_installer_keeps_the_verified_file_from_being_changed() {
        let temp = tempfile::tempdir().unwrap();
        let dir = update_dir(&temp);
        let installer = prepare_installer(&info_for(SHA_ABC), &dir, write_abc).unwrap();
        let other = dir.join("other.exe");
        std::fs::write(&other, b"evil").unwrap();

        let write = std::fs::OpenOptions::new()
            .write(true)
            .open(&installer.path);
        let delete = std::fs::remove_file(&installer.path);
        let replace = std::fs::rename(&other, &installer.path);

        assert!(write.is_err(), "the file was opened for writing");
        assert!(delete.is_err(), "the file was deleted");
        assert!(replace.is_err(), "the file was replaced");
        assert_eq!(std::fs::read(&installer.path).unwrap(), b"abc");
    }

    #[test]
    fn prepare_installer_deletes_a_tampered_download() {
        let temp = tempfile::tempdir().unwrap();
        let dir = update_dir(&temp);

        let result = prepare_installer(&info_for(&"0".repeat(64)), &dir, write_abc);

        assert_eq!(result.err(), Some(UpdateError::DigestMismatch));
        assert!(!dir.exists());
    }

    #[test]
    fn prepare_installer_deletes_the_folder_when_the_download_fails() {
        let temp = tempfile::tempdir().unwrap();
        let dir = update_dir(&temp);

        let result = prepare_installer(&info_for(SHA_ABC), &dir, |file| {
            write_abc(file)?;
            Err(UpdateError::Download("connection reset".into()))
        });

        assert_eq!(
            result.err(),
            Some(UpdateError::Download("connection reset".into()))
        );
        assert!(!dir.exists());
    }

    #[test]
    fn launch_installer_starts_the_file_prepare_installer_holds() {
        let system = std::env::var_os("SystemRoot").expect("SystemRoot");
        let program = std::fs::read(Path::new(&system).join(r"System32\whoami.exe")).unwrap();
        let digest: String = sha2::Sha256::digest(&program)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let temp = tempfile::tempdir().unwrap();
        let dir = update_dir(&temp);
        let installer = prepare_installer(&info_for(&digest), &dir, |file| {
            use std::io::Write;
            file.write_all(&program)
                .map_err(|e| UpdateError::Download(e.to_string()))
        })
        .unwrap();

        // The program rejects the installer switches and exits at once.
        assert_eq!(launch_installer(installer), Ok(()));
    }

    #[test]
    fn remove_stale_update_dirs_deletes_only_update_folders() {
        let temp = tempfile::tempdir().unwrap();
        let stale = temp.path().join("GameFrame-update-42-1");
        let other = temp.path().join("something-else");
        let lookalike_file = temp.path().join("GameFrame-update-file");
        std::fs::create_dir(&stale).unwrap();
        std::fs::write(stale.join("GameFrame-Setup-1.2.0.exe"), b"x").unwrap();
        std::fs::create_dir(&other).unwrap();
        std::fs::write(&lookalike_file, b"x").unwrap();

        let own = temp.path().join("GameFrame-update-7-1");
        std::fs::create_dir(&own).unwrap();

        let left = remove_stale_update_dirs(temp.path(), 7);

        assert_eq!(left, 0);
        assert!(own.exists());
        assert!(!stale.exists());
        assert!(other.exists());
        assert!(lookalike_file.exists());
    }

    /// Downloads a real release asset through GitHub's redirect and checks it
    /// against the digest the release API reports. Needs the network:
    /// `cargo test -- --ignored real_github_download`.
    #[test]
    #[ignore = "needs the network"]
    fn real_github_download_follows_the_redirect_and_verifies() {
        let mut body = Vec::new();

        crate::http::download(
            "https://github.com/cli/cli/releases/download/v2.103.2/gh_2.103.2_checksums.txt",
            &[ASSET_HOST, ASSET_REDIRECT_HOST],
            &mut body,
            MAX_INSTALLER_BYTES,
        )
        .unwrap();

        assert_eq!(body.len(), 1971);
        assert_eq!(
            verify_sha256(
                &body[..],
                "414d6009dc7f9f936d70831af818d1a32aa0ff07a77646861efd207356a36b2d"
            ),
            Ok(())
        );
    }

    #[test]
    #[ignore = "needs the network"]
    fn real_github_download_refuses_an_untrusted_redirect_host() {
        let mut body = Vec::new();

        let result = crate::http::download(
            "https://github.com/cli/cli/releases/download/v2.103.2/gh_2.103.2_checksums.txt",
            &[ASSET_HOST],
            &mut body,
            MAX_INSTALLER_BYTES,
        );

        assert!(result.is_err());
        assert!(body.is_empty());
    }
}
