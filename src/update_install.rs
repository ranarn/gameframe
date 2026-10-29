//! Update install: downloads the installer of an available update into a
//! fresh folder under `%LOCALAPPDATA%\GameFrame\updates`, checks it against
//! the release's SHA-256 (integrity) and its Minisign signature, checked
//! against the public key built into GameFrame (authenticity). A file that
//! fails either, or has no signature, is deleted and never run. Then it starts
//! it silently and detached. An elevated instance never installs: it
//! refuses before anything starts, see [`ensure_unelevated`].
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
    ASSET_HOST, FlagGuard, PUBLIC_KEY, SpawnError, UpdateError, UpdateInfo, ensure_unelevated,
    installer_name, verify_sha256, verify_signature,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// The host `github.com` redirects a release-asset download to (a 302 whose
/// `Location` carries a signed URL).
const ASSET_REDIRECT_HOST: &str = "release-assets.githubusercontent.com";

/// Largest installer a download accepts; the real one is a few MB.
const MAX_INSTALLER_BYTES: u64 = 100 * 1024 * 1024;

/// Largest signature file a download accepts; a real one is about 400 bytes.
const MAX_SIGNATURE_BYTES: u64 = 16 * 1024;

/// Name prefix of the per-install folder an install downloads into.
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

/// A downloaded installer whose digest matched and whose signature verified.
/// `_lock` is the handle they were computed through, opened with a share mode that denies write
/// and delete: while it is open, the file cannot change.
struct VerifiedInstaller {
    path: PathBuf,
    _lock: std::fs::File,
}

/// Writes the installer into `dir` through `download`, then verifies it
/// against the digest of `info` and the Minisign `signature` against
/// `public_key`. Any failure deletes `dir`, so a file that was
/// not verified is never left to be run.
fn prepare_installer(
    info: &UpdateInfo,
    dir: &Path,
    signature: &str,
    public_key: &str,
    download: impl FnOnce(&mut std::fs::File) -> Result<(), UpdateError>,
) -> Result<VerifiedInstaller, UpdateError> {
    use std::io::{Seek, SeekFrom};
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
        // Same handle again, from the start: the signature covers the content
        // that was just hashed.
        (&lock).seek(SeekFrom::Start(0)).map_err(io)?;
        verify_signature(&lock, signature, public_key)?;
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

/// The folder updates download into: `%LOCALAPPDATA%\GameFrame\updates`, which
/// inherits the user profile's ACL (the user, SYSTEM and administrators). A
/// fixed, app-owned location that doesn't depend on a redirectable `%TEMP%`.
/// Not created here.
fn updates_root() -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty())?;
    Some(PathBuf::from(local).join("GameFrame").join("updates"))
}

/// Creates an empty, uniquely named update folder inside `root`.
fn fresh_update_dir_in(root: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(root)?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = root.join(format!("{UPDATE_DIR_PREFIX}{}-{nanos}", std::process::id()));
    std::fs::create_dir(&dir)?;
    Ok(dir)
}

fn fresh_update_dir() -> std::io::Result<PathBuf> {
    let root = updates_root().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "%LOCALAPPDATA% is not set")
    })?;
    fresh_update_dir_in(&root)
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

/// Downloads `url` (vetted like the installer download) into memory as text.
fn download_signature(url: &str) -> Result<String, UpdateError> {
    let mut body = Vec::new();
    crate::http::download(
        url,
        &[ASSET_HOST, ASSET_REDIRECT_HOST],
        &mut body,
        MAX_SIGNATURE_BYTES,
    )
    .map_err(download_error)?;
    String::from_utf8(body)
        .map_err(|_| UpdateError::InvalidSignature("the signature file is not text".into()))
}

fn download_error(e: HttpError) -> UpdateError {
    match e {
        HttpError::Status(status) => UpdateError::Download(format!(
            "the download server answered with HTTP status {status}"
        )),
        HttpError::Failed(e) => UpdateError::Download(e),
    }
}

/// Downloads, verifies and starts the installer of `info`. On any failure the
/// download is deleted and nothing has been run.
fn install(info: &UpdateInfo) -> Result<(), UpdateError> {
    // First, so an unsigned release costs no installer download.
    let signature = download_signature(&info.signature_url)?;
    let dir = fresh_update_dir().map_err(|e| UpdateError::Download(e.to_string()))?;
    let installer = prepare_installer(info, &dir, &signature, PUBLIC_KEY, |file| {
        crate::http::download(
            &info.download_url,
            &[ASSET_HOST, ASSET_REDIRECT_HOST],
            file,
            MAX_INSTALLER_BYTES,
        )
        .map_err(download_error)
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
    // Before anything is reported as started: the installer would inherit this
    // process's token and relaunch GameFrame with it, so an elevated instance
    // installs nothing.
    if ensure_unelevated(crate::win32::is_process_elevated()).is_err() {
        return Err(SpawnError::Elevated);
    }
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

/// Deletes the update folders in `root` except those of the process `own_pid`,
/// whose install may be writing to its folder right now, and returns how many
/// could not be deleted (a running installer holds its file open).
fn remove_stale_update_dirs(root: &Path, own_pid: u32) -> usize {
    let own_prefix = format!("{UPDATE_DIR_PREFIX}{own_pid}-");
    let Ok(entries) = std::fs::read_dir(root) else {
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
            // The old temp location is swept too, for downloads made by
            // earlier versions.
            let roots: Vec<PathBuf> = updates_root()
                .into_iter()
                .chain(std::iter::once(std::env::temp_dir()))
                .collect();
            for _ in 0..ATTEMPTS {
                let left: usize = roots
                    .iter()
                    .map(|root| remove_stale_update_dirs(root, std::process::id()))
                    .sum();
                if left == 0 {
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

    // Content, key and signature made with minisign 0.12 for these tests only
    // (the secret key was thrown away; not GameFrame's key).
    const SIGNED: &[u8] = b"gameframe test installer";
    const KEY: &str = "RWRxNC0yLOQy/hw1eBWJeGOpXKHvQutd1QOylDUvDibdWvEY4xx6/LaW";
    const OTHER_KEY: &str = "RWR5V9eIUOhvVABgQXNW1DdCi23HMnK3p56Px+ldGZbWvci1XG9kIl7Q";
    const SIG: &str = "untrusted comment: signature from minisign secret key\n\
        RURxNC0yLOQy/v4hJSQX0rxIVlzK0PCR2V0+MJrrLOq3mU89i4KZ7HsEZJ9pJbm+rcBADNTUC93/k6zpaqAyCk7hnK1b5PJSiQE=\n\
        trusted comment: timestamp:1793255643\tfile:payload.bin\thashed\n\
        iAxFHuR+2NA1plwGWanAoGiAoGsa9iUir+ka82wHXnO+NB9Bbpa8x/5z7PfxH/z8aPGZ9Fd6UJbgES10AZO2BA==\n";

    fn sha_signed() -> String {
        sha2::Sha256::digest(SIGNED)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn info_for(content_digest: &str) -> UpdateInfo {
        UpdateInfo {
            version: Version::parse("1.2.0").unwrap(),
            download_url: "https://github.com/a.exe".into(),
            sha256: content_digest.into(),
            signature_url: "https://github.com/a.exe.minisig".into(),
        }
    }

    fn write_signed(file: &mut std::fs::File) -> Result<(), UpdateError> {
        use std::io::Write;
        file.write_all(SIGNED)
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

        let installer =
            prepare_installer(&info_for(&sha_signed()), &dir, SIG, KEY, write_signed).unwrap();

        assert_eq!(installer.path, dir.join("GameFrame-Setup-1.2.0.exe"));
        assert_eq!(std::fs::read(&installer.path).unwrap(), SIGNED);
    }

    #[test]
    fn prepare_installer_keeps_the_verified_file_from_being_changed() {
        let temp = tempfile::tempdir().unwrap();
        let dir = update_dir(&temp);
        let installer =
            prepare_installer(&info_for(&sha_signed()), &dir, SIG, KEY, write_signed).unwrap();
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
        assert_eq!(std::fs::read(&installer.path).unwrap(), SIGNED);
    }

    #[test]
    fn prepare_installer_deletes_a_tampered_download() {
        let temp = tempfile::tempdir().unwrap();
        let dir = update_dir(&temp);

        let result = prepare_installer(&info_for(&"0".repeat(64)), &dir, SIG, KEY, write_signed);

        assert_eq!(result.err(), Some(UpdateError::DigestMismatch));
        assert!(!dir.exists());
    }

    #[test]
    fn prepare_installer_deletes_a_download_signed_by_another_key() {
        let temp = tempfile::tempdir().unwrap();
        let dir = update_dir(&temp);

        let result =
            prepare_installer(&info_for(&sha_signed()), &dir, SIG, OTHER_KEY, write_signed);

        assert_eq!(result.err(), Some(UpdateError::SignatureMismatch));
        assert!(!dir.exists());
    }

    #[test]
    fn prepare_installer_deletes_a_tampered_download_whose_digest_was_forged() {
        let temp = tempfile::tempdir().unwrap();
        let dir = update_dir(&temp);
        let evil = b"evil installer";
        let evil_digest: String = sha2::Sha256::digest(evil)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();

        let result = prepare_installer(&info_for(&evil_digest), &dir, SIG, KEY, |file| {
            use std::io::Write;
            file.write_all(evil)
                .map_err(|e| UpdateError::Download(e.to_string()))
        });

        assert_eq!(result.err(), Some(UpdateError::SignatureMismatch));
        assert!(!dir.exists());
    }

    #[test]
    fn prepare_installer_deletes_the_download_without_a_usable_signature() {
        let temp = tempfile::tempdir().unwrap();
        let dir = update_dir(&temp);

        let result = prepare_installer(&info_for(&sha_signed()), &dir, "", KEY, write_signed);

        assert!(matches!(
            result.err(),
            Some(UpdateError::InvalidSignature(_))
        ));
        assert!(!dir.exists());
    }

    #[test]
    fn prepare_installer_deletes_the_folder_when_the_download_fails() {
        let temp = tempfile::tempdir().unwrap();
        let dir = update_dir(&temp);

        let result = prepare_installer(&info_for(&sha_signed()), &dir, SIG, KEY, |file| {
            write_signed(file)?;
            Err(UpdateError::Download("connection reset".into()))
        });

        assert_eq!(
            result.err(),
            Some(UpdateError::Download("connection reset".into()))
        );
        assert!(!dir.exists());
    }

    #[test]
    fn launch_installer_starts_a_locked_installer_file() {
        let system = std::env::var_os("SystemRoot").expect("SystemRoot");
        let program = std::fs::read(Path::new(&system).join(r"System32\whoami.exe")).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let dir = update_dir(&temp);
        let path = dir.join("GameFrame-Setup-1.2.0.exe");
        std::fs::write(&path, &program).unwrap();
        let installer = VerifiedInstaller {
            _lock: std::fs::File::open(&path).unwrap(),
            path,
        };

        // The program rejects the installer switches and exits at once.
        assert_eq!(launch_installer(installer), Ok(()));
    }

    #[test]
    fn fresh_update_dir_in_creates_a_unique_prefixed_folder_and_its_parents() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("GameFrame").join("updates");

        let first = fresh_update_dir_in(&root).unwrap();
        let second = fresh_update_dir_in(&root).unwrap();

        assert_ne!(first, second);
        assert!(first.is_dir() && second.is_dir());
        assert_eq!(first.parent(), Some(root.as_path()));
        assert!(
            first
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(UPDATE_DIR_PREFIX)
        );
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
