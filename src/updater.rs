//! Update check: asks GitHub's "latest release" endpoint whether a release
//! newer than the running build exists, and which installer, SHA-256 digest and
//! Minisign signature belong to it. Installing it is [`crate::update_install`].
//!
//! The decision logic is pure: [`check`] takes the running version and a
//! fetch closure, and [`parse_release`], [`Version`], [`select_asset`],
//! [`select_signature_asset`], [`verify_sha256`] and [`verify_signature`] work
//! on plain strings and readers, so all of it is unit
//! tested without a network. The network part is [`fetch_latest_release`] (a
//! blocking WinHTTP request, see [`crate::http`]) and [`spawn_check`], which
//! runs the whole check on a worker thread so no UI thread ever waits on it.
//!
//! A development build (version `0.0.0`) never fetches anything.

use crate::http::HttpError;
use crate::profile_store::UpdateMode;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};

/// The GitHub API host and the path of the latest non-draft, non-pre-release.
const API_HOST: &str = "api.github.com";
const LATEST_RELEASE_PATH: &str = "/repos/ranarn/gameframe/releases/latest";

/// The version of the running build; `0.0.0` for local and `main` builds.
pub(crate) const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

const INSTALLER_PREFIX: &str = "GameFrame-Setup-";
const INSTALLER_SUFFIX: &str = ".exe";
const DIGEST_PREFIX: &str = "sha256:";
/// Appended to the installer's name for its detached Minisign signature.
const SIGNATURE_SUFFIX: &str = ".minisig";

/// The Minisign public key (the base64 second line of `gameframe.pub`, key id
/// E6CE2092F0EE51EE) every installer must be signed with. The release workflow
/// checks, after signing, that the signature verifies against this exact line
/// (or [`NEXT_PUBLIC_KEY`]), so keep it on one line as
/// `pub(crate) const PUBLIC_KEY: &str = "...";`. To rotate it, see SECURITY.md.
pub(crate) const PUBLIC_KEY: &str = "RWTuUe7wkiDO5jFMnk3TwY6+iUGxIKPqSn70baKHvVFsXwdmCpaK68fS";

/// Second accepted key, empty normally. It only exists to rotate the signing
/// key: a release signed with the old key sets this to the new public key, so
/// that installs made by it accept releases signed with the new key (see
/// SECURITY.md). Keep it on one line as
/// `pub(crate) const NEXT_PUBLIC_KEY: &str = "...";`.
pub(crate) const NEXT_PUBLIC_KEY: &str = "";

/// The keys an installer's signature may verify against.
pub(crate) const TRUSTED_KEYS: [&str; 2] = [PUBLIC_KEY, NEXT_PUBLIC_KEY];

/// The host of an asset's `browser_download_url`.
pub(crate) const ASSET_HOST: &str = "github.com";

/// A plain `x.y.z` version, compared numerically component by component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Version(u32, u32, u32);

impl Version {
    const DEVELOPMENT: Version = Version(0, 0, 0);

    /// Parses `x.y.z`, with one optional leading `v` (a release tag).
    /// Anything else, such as a pre-release suffix, is rejected.
    pub(crate) fn parse(text: &str) -> Result<Self, UpdateError> {
        let invalid = || UpdateError::InvalidVersion(text.to_string());
        let mut parts = text.strip_prefix('v').unwrap_or(text).split('.');
        let mut component = || {
            let part = parts.next().ok_or_else(invalid)?;
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return Err(invalid());
            }
            part.parse::<u32>().map_err(|_| invalid())
        };
        let version = Version(component()?, component()?, component()?);
        if parts.next().is_some() {
            return Err(invalid());
        }
        Ok(version)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// Reasons an update check can fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UpdateError {
    /// The request itself failed (connection, TLS, timeout, body).
    Network(String),
    /// GitHub answered the request with an unexpected HTTP status.
    HttpStatus(u32),
    /// The response is not the expected release JSON.
    Malformed(String),
    /// A version string is not `x.y.z`.
    InvalidVersion(String),
    /// The release has no `GameFrame-Setup-*.exe` asset.
    NoMatchingAsset,
    /// The installer asset carries no `digest`.
    MissingDigest,
    /// The installer asset's `digest` is not `sha256:` plus 64 hex digits.
    InvalidDigest(String),
    /// The installer asset's download URL is not an HTTPS URL on a GitHub
    /// download host.
    UntrustedDownloadUrl(String),
    /// The downloaded file is not the one the release's digest describes.
    DigestMismatch,
    /// The release has no `.minisig` signature next to its installer.
    MissingSignature,
    /// The signature, or the key it is checked against, cannot be read.
    InvalidSignature(String),
    /// The installer is not signed by the key built into GameFrame.
    SignatureMismatch,
    /// The signature is genuine but was made for another installer file, e.g.
    /// an older release's.
    SignatureForOtherFile,
    /// Reading the downloaded file back from disk failed.
    ReadFile(String),
    /// Downloading or storing the installer failed.
    Download(String),
    /// The verified installer could not be started.
    LaunchInstaller(String),
    /// This instance runs elevated and never installs updates (the installer
    /// would inherit the elevated token and relaunch GameFrame elevated).
    ElevatedInstance,
}

/// What an elevated instance tells the user instead of installing.
pub(crate) const ELEVATED_UPDATE_ADVICE: &str = "GameFrame is running as administrator, so it doesn't install updates. Restart GameFrame normally (not as administrator) to update.";

/// Refuses an install while this instance runs elevated.
pub(crate) fn ensure_unelevated(elevated: bool) -> Result<(), UpdateError> {
    if elevated {
        Err(UpdateError::ElevatedInstance)
    } else {
        Ok(())
    }
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpdateError::Network(e) => write!(f, "couldn't reach GitHub: {e}"),
            // A 403 is not proof of a rate limit, so the message only says so
            // as a possibility.
            UpdateError::HttpStatus(status @ (403 | 429)) => {
                write!(
                    f,
                    "GitHub refused the request (HTTP {status}, possibly a rate limit), try again later"
                )
            }
            UpdateError::HttpStatus(status) => {
                write!(f, "GitHub answered with HTTP status {status}")
            }
            UpdateError::Malformed(e) => write!(f, "unexpected response from GitHub: {e}"),
            UpdateError::InvalidVersion(v) => write!(f, "unrecognized version \"{v}\""),
            UpdateError::NoMatchingAsset => {
                write!(
                    f,
                    "the latest release has no {INSTALLER_PREFIX}*.exe installer"
                )
            }
            UpdateError::MissingDigest => {
                write!(f, "the latest release's installer has no SHA-256 digest")
            }
            UpdateError::InvalidDigest(d) => write!(f, "unrecognized installer digest \"{d}\""),
            UpdateError::UntrustedDownloadUrl(u) => {
                write!(f, "the installer's download URL is not a GitHub URL: {u}")
            }
            UpdateError::DigestMismatch => write!(
                f,
                "the downloaded installer does not match the release's SHA-256 digest, so it was not run"
            ),
            UpdateError::MissingSignature => write!(
                f,
                "the latest release's installer has no signature ({SIGNATURE_SUFFIX} file), so it will not be installed"
            ),
            UpdateError::InvalidSignature(e) => {
                write!(f, "the installer's signature can't be checked: {e}")
            }
            UpdateError::SignatureMismatch => write!(
                f,
                "the downloaded installer is not signed by the GameFrame publisher key, so it was not run"
            ),
            UpdateError::SignatureForOtherFile => write!(
                f,
                "the installer's signature was made for a different release, so it was not run"
            ),
            UpdateError::ReadFile(e) => {
                write!(f, "reading the downloaded installer failed: {e}")
            }
            UpdateError::Download(e) => write!(f, "downloading the update failed: {e}"),
            UpdateError::LaunchInstaller(e) => write!(f, "couldn't start the installer: {e}"),
            UpdateError::ElevatedInstance => write!(f, "{ELEVATED_UPDATE_ADVICE}"),
        }
    }
}

impl std::error::Error for UpdateError {}

impl UpdateError {
    /// Whether the update was refused for lack of a valid publisher signature,
    /// the one failure that can mean tampering and so is never left to the log.
    pub(crate) fn is_signature_refusal(&self) -> bool {
        matches!(
            self,
            UpdateError::MissingSignature
                | UpdateError::InvalidSignature(_)
                | UpdateError::SignatureMismatch
                | UpdateError::SignatureForOtherFile
        )
    }
}

/// What the tray balloon says when an update was refused for its signature
/// (`version` when known).
pub(crate) fn signature_refusal_notice(version: Option<Version>) -> String {
    match version {
        Some(v) => format!(
            "The update to v{v} can't be verified and was not installed. \
             Install v{v} in Settings shows why."
        ),
        None => "The latest GameFrame update can't be verified and was not installed. \
                 Check for updates now in Settings shows why."
            .to_string(),
    }
}

/// The installer of a newer release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UpdateInfo {
    pub(crate) version: Version,
    pub(crate) download_url: String,
    /// The expected SHA-256 of the installer, 64 lowercase hex digits.
    pub(crate) sha256: String,
    /// Where the installer's detached Minisign signature is downloaded from.
    pub(crate) signature_url: String,
}

/// What a successful check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CheckOutcome {
    /// The running build is `0.0.0`; nothing was fetched.
    DevelopmentBuild,
    /// GitHub has no published release to compare with.
    NoReleasePublished,
    UpToDate,
    UpdateAvailable(UpdateInfo),
}

impl CheckOutcome {
    /// The one-line text shown to the user.
    pub(crate) fn message(&self) -> String {
        match self {
            CheckOutcome::DevelopmentBuild => {
                "This is a development build; update checks are disabled.".to_string()
            }
            CheckOutcome::NoReleasePublished => {
                "No GameFrame release has been published yet.".to_string()
            }
            CheckOutcome::UpToDate => "GameFrame is up to date.".to_string(),
            CheckOutcome::UpdateAvailable(info) => {
                format!("Update v{} is available.", info.version)
            }
        }
    }
}

/// The text shown to the user for a finished check.
pub(crate) fn result_message(result: &Result<CheckOutcome, UpdateError>) -> String {
    match result {
        Ok(outcome) => outcome.message(),
        Err(e) => format!("Update check failed: {e}"),
    }
}

#[derive(Debug, Deserialize)]
struct RawRelease {
    tag_name: String,
    assets: Vec<RawAsset>,
}

#[derive(Debug, Deserialize)]
struct RawAsset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
}

/// The parts of a GitHub release the updater needs.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Release {
    pub(crate) version: Version,
    assets: Vec<Asset>,
}

#[derive(Debug, PartialEq, Eq)]
struct Asset {
    name: String,
    download_url: String,
    digest: Option<String>,
}

/// Parses the JSON of `GET /repos/{owner}/{repo}/releases/latest`.
pub(crate) fn parse_release(json: &str) -> Result<Release, UpdateError> {
    let raw: RawRelease =
        serde_json::from_str(json).map_err(|e| UpdateError::Malformed(e.to_string()))?;
    Ok(Release {
        version: Version::parse(&raw.tag_name)?,
        assets: raw
            .assets
            .into_iter()
            .map(|a| Asset {
                name: a.name,
                download_url: a.browser_download_url,
                digest: a.digest,
            })
            .collect(),
    })
}

/// Whether `url` is HTTPS on exactly [`ASSET_HOST`] (no userinfo, port or
/// look-alike host).
fn is_trusted_download_url(url: &str) -> bool {
    crate::http::split_https_url(url).is_some_and(|(host, _)| host == ASSET_HOST)
}

/// The file name of the installer of `version`.
pub(crate) fn installer_name(version: Version) -> String {
    format!("{INSTALLER_PREFIX}{version}{INSTALLER_SUFFIX}")
}

/// Picks the `GameFrame-Setup-<release version>.exe` asset (not its `.sha256`
/// sibling, the notices or an installer of another version) and returns its
/// download URL and expected SHA-256.
pub(crate) fn select_asset(release: &Release) -> Result<(String, String), UpdateError> {
    let installer_name = installer_name(release.version);
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == installer_name)
        .ok_or(UpdateError::NoMatchingAsset)?;
    if !is_trusted_download_url(&asset.download_url) {
        return Err(UpdateError::UntrustedDownloadUrl(
            asset.download_url.clone(),
        ));
    }
    let digest = asset.digest.as_deref().ok_or(UpdateError::MissingDigest)?;
    let hex = digest
        .strip_prefix(DIGEST_PREFIX)
        .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| UpdateError::InvalidDigest(digest.to_string()))?;
    Ok((asset.download_url.clone(), hex.to_ascii_lowercase()))
}

/// Picks the `<installer>.minisig` asset of the release and returns its
/// download URL (vetted like the installer's). Its absence is an error: an
/// unsigned installer is never installed.
pub(crate) fn select_signature_asset(release: &Release) -> Result<String, UpdateError> {
    let name = format!("{}{SIGNATURE_SUFFIX}", installer_name(release.version));
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == name)
        .ok_or(UpdateError::MissingSignature)?;
    if !is_trusted_download_url(&asset.download_url) {
        return Err(UpdateError::UntrustedDownloadUrl(
            asset.download_url.clone(),
        ));
    }
    Ok(asset.download_url.clone())
}

/// The trusted comment the release workflow signs `installer_file` with. The
/// signature covers it, so a valid signature of an older installer cannot be
/// passed off as the one of a newer release.
pub(crate) fn expected_trusted_comment(installer_file: &str) -> String {
    format!("file:{installer_file}")
}

/// Checks that the bytes `reader` yield were signed with one of the Minisign
/// keys `public_keys` (the base64 line of `minisign.pub`; empty entries are
/// ignored) and that the signature was made for `installer_file`, i.e. its
/// trusted comment is [`expected_trusted_comment`]. `signature` is the text of
/// the `.minisig` file. Authenticity, as opposed to [`verify_sha256`]'s
/// integrity. Streams the data; only pre-hashed signatures (minisign's default)
/// are accepted, never the legacy format.
pub(crate) fn verify_signature(
    mut reader: impl Read,
    signature: &str,
    public_keys: &[&str],
    installer_file: &str,
) -> Result<(), UpdateError> {
    use minisign_verify::{PublicKey, Signature};
    let signature = Signature::decode(signature)
        .map_err(|e| UpdateError::InvalidSignature(format!("malformed signature ({e})")))?;
    let mut keys = Vec::new();
    let mut key_error = None;
    for text in public_keys.iter().filter(|k| !k.is_empty()) {
        match PublicKey::from_base64(text) {
            Ok(key) => keys.push(key),
            Err(e) => key_error = Some(format!("no usable public key ({e})")),
        }
    }
    // Only the key the signature names accepts it.
    let mut verifier = match keys.iter().find_map(|k| k.verify_stream(&signature).ok()) {
        Some(verifier) => verifier,
        None => {
            // A parse error is only the cause when no key parsed at all.
            return Err(if keys.is_empty() {
                UpdateError::InvalidSignature(
                    key_error.unwrap_or_else(|| "no public key configured".into()),
                )
            } else {
                UpdateError::SignatureMismatch
            });
        }
    };
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut chunk)
            .map_err(|e| UpdateError::ReadFile(e.to_string()))?;
        if read == 0 {
            break;
        }
        verifier.update(&chunk[..read]);
    }
    verifier
        .finalize()
        .map_err(|_| UpdateError::SignatureMismatch)?;
    // The trusted comment is covered by the signature just verified.
    if signature.trusted_comment() != expected_trusted_comment(installer_file) {
        return Err(UpdateError::SignatureForOtherFile);
    }
    Ok(())
}

/// Runs an update check for the build `current_version`. `fetch` returns the
/// latest-release JSON, or `None` when no release has been published, and is
/// not called at all for a development build.
pub(crate) fn check(
    current_version: &str,
    fetch: impl FnOnce() -> Result<Option<String>, UpdateError>,
) -> Result<CheckOutcome, UpdateError> {
    let current = Version::parse(current_version)?;
    if current == Version::DEVELOPMENT {
        return Ok(CheckOutcome::DevelopmentBuild);
    }
    let Some(body) = fetch()? else {
        return Ok(CheckOutcome::NoReleasePublished);
    };
    let release = parse_release(&body)?;
    if release.version <= current {
        return Ok(CheckOutcome::UpToDate);
    }
    let (download_url, sha256) = select_asset(&release)?;
    let signature_url = select_signature_asset(&release)?;
    Ok(CheckOutcome::UpdateAvailable(UpdateInfo {
        version: release.version,
        download_url,
        sha256,
        signature_url,
    }))
}

/// What the startup check does with its result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StartupAction {
    /// Nothing to install or offer. A failed check ends up here too: it is
    /// only logged (by [`spawn_check`]), never shown, and not retried.
    Nothing,
    /// Install this update now, without asking.
    InstallNow(UpdateInfo),
    /// Offer this update as a tray item.
    OfferInTray(UpdateInfo),
    /// An update exists but this instance runs elevated: tell the user to
    /// restart GameFrame normally; install nothing.
    ReportElevated(UpdateInfo),
}

/// Decides what to do with the result of the startup check under `mode`.
/// An `elevated` instance never installs, whatever the mode.
pub(crate) fn startup_action(
    mode: UpdateMode,
    elevated: bool,
    result: Result<CheckOutcome, UpdateError>,
) -> StartupAction {
    match (result, mode) {
        (Ok(CheckOutcome::UpdateAvailable(info)), _) if elevated => {
            StartupAction::ReportElevated(info)
        }
        (Ok(CheckOutcome::UpdateAvailable(info)), UpdateMode::Auto) => {
            StartupAction::InstallNow(info)
        }
        (Ok(CheckOutcome::UpdateAvailable(info)), UpdateMode::Ask) => {
            StartupAction::OfferInTray(info)
        }
        _ => StartupAction::Nothing,
    }
}

/// Fetches the latest-release JSON from GitHub; `None` when no release has been
/// published (a 404). Blocking; never call it on a UI thread (see
/// [`spawn_check`]).
fn fetch_latest_release() -> Result<Option<String>, UpdateError> {
    match crate::http::get_text(API_HOST, LATEST_RELEASE_PATH) {
        Ok(body) => Ok(Some(body)),
        Err(HttpError::Status(404)) => Ok(None),
        Err(HttpError::Status(status)) => Err(UpdateError::HttpStatus(status)),
        Err(HttpError::Failed(e)) => Err(UpdateError::Network(e)),
    }
}

/// Whether a check started by [`spawn_check`] is still running.
static CHECK_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Holds a flag set; clears it on drop, so a panicking worker or a failed
/// thread spawn releases it too.
pub(crate) struct FlagGuard(&'static AtomicBool);

impl FlagGuard {
    /// `None` while another holder has the flag.
    pub(crate) fn acquire(flag: &'static AtomicBool) -> Option<Self> {
        (!flag.swap(true, Ordering::AcqRel)).then_some(Self(flag))
    }
}

impl Drop for FlagGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Why a worker thread (check or install) was not started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpawnError {
    /// An earlier one is still running.
    Busy,
    /// The operating system refused the thread.
    ThreadFailed,
    /// An install was asked for by an elevated instance, which never installs.
    Elevated,
}

/// Runs a check for the running build on a worker thread and hands the
/// result to `on_done` there. Logs a failure. Starts nothing while an earlier
/// check is still fetching; the flag is released before `on_done` runs, so a
/// slow `on_done` (a message box) blocks nothing.
pub(crate) fn spawn_check(
    on_done: impl FnOnce(Result<CheckOutcome, UpdateError>) + Send + 'static,
) -> Result<(), SpawnError> {
    let Some(in_flight) = FlagGuard::acquire(&CHECK_IN_FLIGHT) else {
        return Err(SpawnError::Busy);
    };
    let spawned = std::thread::Builder::new()
        .name("update-check".into())
        .spawn(move || {
            let result = check(CURRENT_VERSION, fetch_latest_release);
            drop(in_flight);
            if let Err(e) = &result {
                log!("update check failed: {e}");
            }
            on_done(result);
        });
    spawned.map(drop).map_err(|e| {
        log!("update check: couldn't start the worker thread: {e}");
        SpawnError::ThreadFailed
    })
}

/// Checks that the bytes `reader` yields hash to `expected` (64 hex digits,
/// either case). Streams the data; nothing is buffered.
pub(crate) fn verify_sha256(mut reader: impl Read, expected: &str) -> Result<(), UpdateError> {
    if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(UpdateError::InvalidDigest(expected.to_string()));
    }
    let mut hasher = Sha256::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut chunk)
            .map_err(|e| UpdateError::ReadFile(e.to_string()))?;
        if read == 0 {
            break;
        }
        hasher.update(&chunk[..read]);
    }
    let actual: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(UpdateError::DigestMismatch)
    }
}

/// Minisign test vectors shared by the `updater` and `update_install` tests.
/// Generated for these tests only (Ed25519 over the BLAKE2b-512 prehash, the
/// minisign file format); the secret keys were thrown away. Not GameFrame's key.
#[cfg(test)]
pub(crate) mod test_signing {
    /// What every signature below signs.
    pub(crate) const SIGNED_CONTENT: &[u8] = b"gameframe test installer";
    /// The installer file name the main signatures were made for (version 1.2.0).
    pub(crate) const INSTALLER: &str = "GameFrame-Setup-1.2.0.exe";
    pub(crate) const PUBLIC: &str = "RWRrGo2Vw3gmNJZptjsa/37+ZTtVrbFkkXcc+XyrVUCUxAPbS7k6d5L5";
    pub(crate) const OTHER_PUBLIC: &str =
        "RWTsnKflULaSFmYA5FZZdOdzaAD97A0iJj6lOGFGKg+17gEHhjSxRzPx";
    pub(crate) const NEXT_PUBLIC: &str = "RWQhJM6CesdFX7EFgT+SQpIi9dxocDra/bRxn1lIkKekAC968AOXW1vj";
    /// By [`PUBLIC`]'s key, trusted comment `file:GameFrame-Setup-1.2.0.exe`.
    pub(crate) const SIGNATURE: &str = "untrusted comment: signature from minisign secret key\n\
        RURrGo2Vw3gmNFJujZcPLpNOs/uNSEA368saYb5N7XeK/hw3OsWNCiAjTlHYSHAE09Fgg8mzUEBpADXhwr36cQfwp8dc6eCcfwk=\n\
        trusted comment: file:GameFrame-Setup-1.2.0.exe\n\
        WGe2I1VGTUuKZNOoTB+hYf3p2DyI4IxCp3mzO/+13XBRig+fmU9xq5LJ2DRtr2IKEQvBHP7LY+WQ5p829gu8AA==\n";
    /// By [`PUBLIC`]'s key, trusted comment `file:GameFrame-Setup-1.1.0.exe`.
    pub(crate) const SIGNATURE_FOR_1_1_0: &str = "untrusted comment: signature from minisign secret key\n\
        RURrGo2Vw3gmNFJujZcPLpNOs/uNSEA368saYb5N7XeK/hw3OsWNCiAjTlHYSHAE09Fgg8mzUEBpADXhwr36cQfwp8dc6eCcfwk=\n\
        trusted comment: file:GameFrame-Setup-1.1.0.exe\n\
        mRnWB3t+DxL6cYca1CQboHDpxA1aXPrsZRhdmEl2mcCnoyITM5fEzz9zDHXK9WIZc2ohozCldO9GpnJXyHNyCA==\n";
    /// By [`NEXT_PUBLIC`]'s key, trusted comment `file:GameFrame-Setup-1.2.0.exe`.
    pub(crate) const SIGNATURE_BY_NEXT_KEY: &str = "untrusted comment: signature from minisign secret key\n\
        RUQhJM6CesdFX3EqyoOzzl3aqspV7h9gOMFgF8zhOs6UacG6UidPTzOCXy+paIAiLgzFOkCXAwxS+awbKsd3lV6AyudDQqrQ0wE=\n\
        trusted comment: file:GameFrame-Setup-1.2.0.exe\n\
        uVUZ4gW1ZaOwEquutsIVGqkA3JDjiOkpLdi67pZ3Z62pZ7hO4x0dbeS1IWoyVSkwo6j+zV/8s1B84vRhEA2vBg==\n";
}
#[cfg(test)]
mod tests {
    use super::test_signing::*;
    use super::*;
    use std::cell::Cell;

    const SHA_A: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// A real `releases/latest` response (cli/cli v2.103.2), verbatim except
    /// that `assets` is cut down to two of its 22 entries.
    const REAL_RELEASE: &str = r####"{"url":"https://api.github.com/repos/cli/cli/releases/422277825","assets_url":"https://api.github.com/repos/cli/cli/releases/422277825/assets","upload_url":"https://uploads.github.com/repos/cli/cli/releases/422277825/assets{?name,label}","html_url":"https://github.com/cli/cli/releases/tag/v2.103.2","id":422277825,"author":{"login":"github-actions[bot]","id":41898282,"node_id":"MDM6Qm90NDE4OTgyODI=","avatar_url":"https://avatars.githubusercontent.com/in/15368?v=4","gravatar_id":"","url":"https://api.github.com/users/github-actions%5Bbot%5D","html_url":"https://github.com/apps/github-actions","followers_url":"https://api.github.com/users/github-actions%5Bbot%5D/followers","following_url":"https://api.github.com/users/github-actions%5Bbot%5D/following{/other_user}","gists_url":"https://api.github.com/users/github-actions%5Bbot%5D/gists{/gist_id}","starred_url":"https://api.github.com/users/github-actions%5Bbot%5D/starred{/owner}{/repo}","subscriptions_url":"https://api.github.com/users/github-actions%5Bbot%5D/subscriptions","organizations_url":"https://api.github.com/users/github-actions%5Bbot%5D/orgs","repos_url":"https://api.github.com/users/github-actions%5Bbot%5D/repos","events_url":"https://api.github.com/users/github-actions%5Bbot%5D/events{/privacy}","received_events_url":"https://api.github.com/users/github-actions%5Bbot%5D/received_events","type":"Bot","user_view_type":"public","site_admin":false},"node_id":"RE_kwDODKw3uc4ZK3LB","tag_name":"v2.103.2","target_commitish":"4f2cfe92e3499f8581bf39db77e0a1431fa7b55c","name":"GitHub CLI 2.103.2","draft":false,"immutable":true,"prerelease":false,"created_at":"2026-10-23T20:08:01Z","updated_at":"2026-10-23T20:28:28Z","published_at":"2026-10-23T20:24:12Z","assets":[{"url":"https://api.github.com/repos/cli/cli/releases/assets/670305018","id":670305018,"node_id":"RA_kwDODKw3uc4n9Ar6","name":"gh_2.103.2_checksums.txt","label":"GitHub CLI 2.103.2 checksums","uploader":{"login":"github-actions[bot]","id":41898282,"node_id":"MDM6Qm90NDE4OTgyODI=","avatar_url":"https://avatars.githubusercontent.com/in/15368?v=4","gravatar_id":"","url":"https://api.github.com/users/github-actions%5Bbot%5D","html_url":"https://github.com/apps/github-actions","followers_url":"https://api.github.com/users/github-actions%5Bbot%5D/followers","following_url":"https://api.github.com/users/github-actions%5Bbot%5D/following{/other_user}","gists_url":"https://api.github.com/users/github-actions%5Bbot%5D/gists{/gist_id}","starred_url":"https://api.github.com/users/github-actions%5Bbot%5D/starred{/owner}{/repo}","subscriptions_url":"https://api.github.com/users/github-actions%5Bbot%5D/subscriptions","organizations_url":"https://api.github.com/users/github-actions%5Bbot%5D/orgs","repos_url":"https://api.github.com/users/github-actions%5Bbot%5D/repos","events_url":"https://api.github.com/users/github-actions%5Bbot%5D/events{/privacy}","received_events_url":"https://api.github.com/users/github-actions%5Bbot%5D/received_events","type":"Bot","user_view_type":"public","site_admin":false},"content_type":"text/plain; charset=utf-8","state":"uploaded","size":1971,"digest":"sha256:414d6009dc7f9f936d70831af818d1a32aa0ff07a77646861efd207356a36b2d","download_count":13395,"created_at":"2026-10-23T20:24:05Z","updated_at":"2026-10-23T20:24:05Z","browser_download_url":"https://github.com/cli/cli/releases/download/v2.103.2/gh_2.103.2_checksums.txt"},{"url":"https://api.github.com/repos/cli/cli/releases/assets/670305194","id":670305194,"node_id":"RA_kwDODKw3uc4n9Auq","name":"gh_2.103.2_windows_amd64.msi","label":"GitHub CLI 2.103.2 windows amd64 installer","uploader":{"login":"github-actions[bot]","id":41898282,"node_id":"MDM6Qm90NDE4OTgyODI=","avatar_url":"https://avatars.githubusercontent.com/in/15368?v=4","gravatar_id":"","url":"https://api.github.com/users/github-actions%5Bbot%5D","html_url":"https://github.com/apps/github-actions","followers_url":"https://api.github.com/users/github-actions%5Bbot%5D/followers","following_url":"https://api.github.com/users/github-actions%5Bbot%5D/following{/other_user}","gists_url":"https://api.github.com/users/github-actions%5Bbot%5D/gists{/gist_id}","starred_url":"https://api.github.com/users/github-actions%5Bbot%5D/starred{/owner}{/repo}","subscriptions_url":"https://api.github.com/users/github-actions%5Bbot%5D/subscriptions","organizations_url":"https://api.github.com/users/github-actions%5Bbot%5D/orgs","repos_url":"https://api.github.com/users/github-actions%5Bbot%5D/repos","events_url":"https://api.github.com/users/github-actions%5Bbot%5D/events{/privacy}","received_events_url":"https://api.github.com/users/github-actions%5Bbot%5D/received_events","type":"Bot","user_view_type":"public","site_admin":false},"content_type":"application/x-msi","state":"uploaded","size":15446016,"digest":"sha256:7560d893608d4244bf57e06af03f742ae3abed3b88169304eec77b77c0dc7598","download_count":61825,"created_at":"2026-10-23T20:24:09Z","updated_at":"2026-10-23T20:24:10Z","browser_download_url":"https://github.com/cli/cli/releases/download/v2.103.2/gh_2.103.2_windows_amd64.msi"}],"tarball_url":"https://api.github.com/repos/cli/cli/tarball/v2.103.2","zipball_url":"https://api.github.com/repos/cli/cli/zipball/v2.103.2","body":"### 🐛 Fixes\r\n\r\n* Allow server to arbitrate which GitHub App installation tokens can attach files by @martinajir in [#14804](https://github.com/cli/cli/pull/14804)\r\n* Defer `--attach` credential checks to the upload endpoint by @BagToad in [#14805](https://github.com/cli/cli/pull/14805)\r\n\r\n## New Contributors\r\n* @martinajir made their first contribution in https://github.com/cli/cli/pull/14804\r\n\r\n**Full Changelog**: https://github.com/cli/cli/compare/v2.103.1...v2.103.2","reactions":{"url":"https://api.github.com/repos/cli/cli/releases/422277825/reactions","total_count":4,"+1":4,"-1":0,"laugh":0,"hooray":0,"confused":0,"heart":0,"rocket":0,"eyes":0},"mentions_count":2}"####;

    /// A hand-built release shaped like GameFrame's, for asset selection.
    const FULL_RELEASE: &str = r#"{
  "url": "https://api.github.com/repos/ranarn/gameframe/releases/100000001",
  "html_url": "https://github.com/ranarn/gameframe/releases/tag/v1.2.0",
  "id": 100000001,
  "tag_name": "v1.2.0",
  "name": "GameFrame 1.2.0",
  "draft": false,
  "prerelease": false,
  "created_at": "2026-10-01T10:00:00Z",
  "published_at": "2026-10-01T10:05:00Z",
  "assets": [
    {
      "id": 1,
      "name": "GameFrame-Setup-1.2.0.exe",
      "content_type": "application/octet-stream",
      "state": "uploaded",
      "size": 6500000,
      "digest": "sha256:0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef",
      "browser_download_url": "https://github.com/ranarn/gameframe/releases/download/v1.2.0/GameFrame-Setup-1.2.0.exe"
    },
    {
      "id": 2,
      "name": "GameFrame-Setup-1.2.0.exe.sha256",
      "digest": "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
      "browser_download_url": "https://github.com/ranarn/gameframe/releases/download/v1.2.0/GameFrame-Setup-1.2.0.exe.sha256"
    },
    {
      "id": 4,
      "name": "GameFrame-Setup-1.2.0.exe.minisig",
      "digest": null,
      "browser_download_url": "https://github.com/ranarn/gameframe/releases/download/v1.2.0/GameFrame-Setup-1.2.0.exe.minisig"
    },
    {
      "id": 3,
      "name": "THIRD-PARTY-NOTICES.html",
      "digest": null,
      "browser_download_url": "https://github.com/ranarn/gameframe/releases/download/v1.2.0/THIRD-PARTY-NOTICES.html"
    }
  ],
  "body": "Release notes"
}"#;

    fn release_json(tag: &str, assets: &str) -> String {
        format!(r#"{{"tag_name": "{tag}", "assets": [{assets}]}}"#)
    }

    fn asset_json(name: &str, digest: Option<&str>) -> String {
        let digest = digest.map_or("null".to_string(), |d| format!("\"{d}\""));
        format!(
            r#"{{"name": "{name}", "digest": {digest}, "browser_download_url": "https://github.com/ranarn/gameframe/releases/download/v1.2.0/{name}"}}"#
        )
    }

    /// The installer asset and its `.minisig`.
    fn installer_json(version: &str) -> String {
        [
            asset_json(
                &format!("GameFrame-Setup-{version}.exe"),
                Some(&format!("sha256:{SHA_A}")),
            ),
            asset_json(&format!("GameFrame-Setup-{version}.exe.minisig"), None),
        ]
        .join(",")
    }

    fn check_json(current: &str, json: &str) -> Result<CheckOutcome, UpdateError> {
        check(current, || Ok(Some(json.to_string())))
    }

    #[test]
    fn parse_release_reads_the_tag_and_assets_of_a_full_response() {
        let release = parse_release(FULL_RELEASE).unwrap();

        assert_eq!(release.version, Version(1, 2, 0));
        let (url, sha256) = select_asset(&release).unwrap();
        assert_eq!(
            url,
            "https://github.com/ranarn/gameframe/releases/download/v1.2.0/GameFrame-Setup-1.2.0.exe"
        );
        assert_eq!(sha256, SHA_A);
    }

    #[test]
    fn parse_release_reads_the_tag_and_assets_of_a_real_github_response() {
        let release = parse_release(REAL_RELEASE).unwrap();

        assert_eq!(release.version, Version(2, 103, 2));
        assert_eq!(release.assets.len(), 2);
        assert_eq!(release.assets[1].name, "gh_2.103.2_windows_amd64.msi");
        assert_eq!(
            release.assets[1].digest.as_deref(),
            Some("sha256:7560d893608d4244bf57e06af03f742ae3abed3b88169304eec77b77c0dc7598")
        );
        assert_eq!(select_asset(&release), Err(UpdateError::NoMatchingAsset));
    }

    #[test]
    fn parse_release_rejects_json_that_is_not_a_release() {
        assert!(matches!(
            parse_release("not json"),
            Err(UpdateError::Malformed(_))
        ));
        assert!(matches!(
            parse_release(r#"{"message": "Not Found", "status": "404"}"#),
            Err(UpdateError::Malformed(_))
        ));
    }

    #[test]
    fn parse_release_rejects_a_missing_tag_or_asset_list() {
        assert!(matches!(
            parse_release(r#"{"assets": []}"#),
            Err(UpdateError::Malformed(_))
        ));
        assert!(matches!(
            parse_release(r#"{"tag_name": "v1.0.0"}"#),
            Err(UpdateError::Malformed(_))
        ));
    }

    #[test]
    fn parse_release_rejects_an_asset_without_a_download_url() {
        let json = release_json("v1.0.0", r#"{"name": "GameFrame-Setup-1.0.0.exe"}"#);

        assert!(matches!(
            parse_release(&json),
            Err(UpdateError::Malformed(_))
        ));
    }

    #[test]
    fn parse_release_rejects_a_tag_that_is_not_a_plain_version() {
        let json = release_json("nightly", "");

        assert_eq!(
            parse_release(&json),
            Err(UpdateError::InvalidVersion("nightly".into()))
        );
    }

    #[test]
    fn version_parse_accepts_plain_and_v_prefixed_versions() {
        assert_eq!(Version::parse("1.2.3"), Ok(Version(1, 2, 3)));
        assert_eq!(Version::parse("v1.2.3"), Ok(Version(1, 2, 3)));
        assert_eq!(Version::parse("0.0.0"), Ok(Version(0, 0, 0)));
    }

    #[test]
    fn version_parse_rejects_anything_but_three_numeric_components() {
        for bad in [
            "",
            "v",
            "1",
            "1.2",
            "1.2.3.4",
            "1.2.x",
            "1..3",
            "1.2.3-rc1",
            "vv1.2.3",
            "V1.2.3",
            "+1.2.3",
            " 1.2.3",
            "1.2.3 ",
            "99999999999.0.0",
        ] {
            assert!(Version::parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn versions_compare_numerically_per_component() {
        assert!(Version(1, 2, 4) > Version(1, 2, 3));
        assert!(Version(1, 3, 0) > Version(1, 2, 9));
        assert!(Version(2, 0, 0) > Version(1, 99, 99));
        assert!(Version(1, 10, 0) > Version(1, 9, 0));
        assert!(Version(1, 2, 10) > Version(1, 2, 9));
        assert_eq!(Version(1, 2, 3), Version(1, 2, 3));
    }

    #[test]
    fn check_reports_an_update_for_a_newer_release() {
        let json = release_json("v1.2.0", &installer_json("1.2.0"));

        let outcome = check_json("1.1.9", &json).unwrap();

        assert_eq!(
            outcome,
            CheckOutcome::UpdateAvailable(UpdateInfo {
                version: Version(1, 2, 0),
                download_url: "https://github.com/ranarn/gameframe/releases/download/v1.2.0/GameFrame-Setup-1.2.0.exe".into(),
                sha256: SHA_A.into(),
                signature_url: "https://github.com/ranarn/gameframe/releases/download/v1.2.0/GameFrame-Setup-1.2.0.exe.minisig".into(),
            })
        );
    }

    #[test]
    fn check_compares_multi_digit_components_numerically() {
        let json = release_json("v1.10.0", &installer_json("1.10.0"));

        assert!(matches!(
            check_json("1.9.0", &json),
            Ok(CheckOutcome::UpdateAvailable(_))
        ));
    }

    #[test]
    fn check_is_up_to_date_for_an_equal_or_older_release() {
        let json = release_json("v1.2.0", &installer_json("1.2.0"));

        assert_eq!(check_json("1.2.0", &json), Ok(CheckOutcome::UpToDate));
        assert_eq!(check_json("1.3.0", &json), Ok(CheckOutcome::UpToDate));
    }

    #[test]
    fn check_does_not_need_an_installer_when_already_up_to_date() {
        let json = release_json("v1.2.0", "");

        assert_eq!(check_json("1.2.0", &json), Ok(CheckOutcome::UpToDate));
    }

    #[test]
    fn check_on_a_development_build_never_fetches() {
        let fetched = Cell::new(false);

        let outcome = check("0.0.0", || {
            fetched.set(true);
            Ok(Some(FULL_RELEASE.to_string()))
        });

        assert_eq!(outcome, Ok(CheckOutcome::DevelopmentBuild));
        assert!(!fetched.get());
    }

    #[test]
    fn check_reports_a_missing_release_as_an_outcome_not_an_error() {
        let outcome = check("1.0.0", || Ok(None));

        assert_eq!(outcome, Ok(CheckOutcome::NoReleasePublished));
    }

    #[test]
    fn rate_limit_and_other_statuses_do_not_claim_github_was_unreachable() {
        for status in [403, 429, 500] {
            let message = UpdateError::HttpStatus(status).to_string();
            assert!(!message.contains("reach"), "{message}");
        }
        let forbidden = UpdateError::HttpStatus(403).to_string();
        assert!(forbidden.contains("possibly a rate limit"), "{forbidden}");
        assert!(forbidden.contains("403"), "{forbidden}");
        assert!(UpdateError::HttpStatus(500).to_string().contains("500"));
    }

    #[test]
    fn check_passes_a_fetch_failure_through() {
        let outcome = check("1.0.0", || Err(UpdateError::Network("timed out".into())));

        assert_eq!(outcome, Err(UpdateError::Network("timed out".into())));
    }

    #[test]
    fn check_rejects_an_unparseable_running_version() {
        assert!(matches!(
            check("garbage", || Ok(Some(FULL_RELEASE.to_string()))),
            Err(UpdateError::InvalidVersion(_))
        ));
    }

    #[test]
    fn select_asset_picks_the_installer_among_several_assets() {
        let assets = [
            asset_json("THIRD-PARTY-NOTICES.html", None),
            asset_json(
                "GameFrame-Setup-1.2.0.exe.sha256",
                Some(&format!("sha256:{SHA_A}")),
            ),
            asset_json(
                "GameFrame-Portable-1.2.0.exe",
                Some(&format!("sha256:{SHA_A}")),
            ),
            asset_json(
                "GameFrame-Setup-1.2.0.exe",
                Some(&format!("sha256:{}", "b".repeat(64))),
            ),
        ]
        .join(",");
        let release = parse_release(&release_json("v1.2.0", &assets)).unwrap();

        let (url, sha256) = select_asset(&release).unwrap();

        assert_eq!(
            url,
            "https://github.com/ranarn/gameframe/releases/download/v1.2.0/GameFrame-Setup-1.2.0.exe"
        );
        assert_eq!(sha256, "b".repeat(64));
    }

    #[test]
    fn select_asset_fails_without_a_matching_asset() {
        let assets = [
            asset_json(
                "GameFrame-Setup-1.2.0.exe.sha256",
                Some(&format!("sha256:{SHA_A}")),
            ),
            asset_json("notes.txt", None),
        ]
        .join(",");
        let release = parse_release(&release_json("v1.2.0", &assets)).unwrap();

        assert_eq!(select_asset(&release), Err(UpdateError::NoMatchingAsset));
        let empty = parse_release(&release_json("v1.2.0", "")).unwrap();
        assert_eq!(select_asset(&empty), Err(UpdateError::NoMatchingAsset));
    }

    #[test]
    fn select_asset_requires_the_installer_of_the_release_version() {
        for name in [
            "GameFrame-Setup-.exe",
            "GameFrame-Setup-1.1.0.exe",
            "GameFrame-Setup-1.2.0-beta.exe",
        ] {
            let assets = asset_json(name, Some(&format!("sha256:{SHA_A}")));
            let release = parse_release(&release_json("v1.2.0", &assets)).unwrap();

            assert_eq!(
                select_asset(&release),
                Err(UpdateError::NoMatchingAsset),
                "{name}"
            );
        }
    }

    #[test]
    fn select_asset_fails_when_the_installer_has_no_digest() {
        let assets = asset_json("GameFrame-Setup-1.2.0.exe", None);
        let release = parse_release(&release_json("v1.2.0", &assets)).unwrap();

        assert_eq!(select_asset(&release), Err(UpdateError::MissingDigest));
    }

    #[test]
    fn select_asset_rejects_an_odd_digest() {
        for digest in [
            "".to_string(),
            "sha256:".to_string(),
            SHA_A.to_string(),
            format!("sha1:{SHA_A}"),
            format!("sha256:{}", &SHA_A[..63]),
            format!("sha256:{SHA_A}0"),
            format!("sha256:{}g", &SHA_A[..63]),
        ] {
            let assets = asset_json("GameFrame-Setup-1.2.0.exe", Some(&digest));
            let release = parse_release(&release_json("v1.2.0", &assets)).unwrap();

            assert_eq!(
                select_asset(&release),
                Err(UpdateError::InvalidDigest(digest.clone())),
                "{digest:?}"
            );
        }
    }

    #[test]
    fn select_asset_lowercases_the_digest() {
        let upper = SHA_A.to_ascii_uppercase();
        let assets = asset_json(
            "GameFrame-Setup-1.2.0.exe",
            Some(&format!("sha256:{upper}")),
        );
        let release = parse_release(&release_json("v1.2.0", &assets)).unwrap();

        assert_eq!(select_asset(&release).unwrap().1, SHA_A);
    }

    fn select_with_url(url: &str) -> Result<(String, String), UpdateError> {
        let json = format!(
            r#"{{"tag_name": "v1.2.0", "assets": [{{"name": "GameFrame-Setup-1.2.0.exe",
            "digest": "sha256:{SHA_A}", "browser_download_url": "{url}"}}]}}"#
        );
        select_asset(&parse_release(&json).unwrap())
    }

    #[test]
    fn select_asset_accepts_https_urls_on_the_asset_host() {
        for url in [
            "https://github.com/ranarn/gameframe/releases/download/v1.2.0/GameFrame-Setup-1.2.0.exe",
            "https://github.com/x/GameFrame-Setup-1.2.0.exe?a=b",
        ] {
            assert_eq!(select_with_url(url).unwrap().0, url);
        }
    }

    #[test]
    fn select_asset_rejects_a_download_url_off_github_or_not_https() {
        for url in [
            "http://github.com/a.exe",
            "https://example.test/a.exe",
            "https://github.com.evil.test/a.exe",
            "https://evil.test/github.com/a.exe",
            "https://github.com@evil.test/a.exe",
            "https://evil.test#@github.com/a.exe",
            "https://github.com:8443/a.exe",
            "https://notgithub.com/a.exe",
            "https://release-assets.githubusercontent.com/a.exe",
            "",
        ] {
            assert!(
                matches!(
                    select_with_url(url),
                    Err(UpdateError::UntrustedDownloadUrl(_))
                ),
                "{url:?} should be rejected"
            );
        }
    }

    #[test]
    fn flag_guard_holds_the_flag_until_dropped_even_by_a_panic() {
        static FLAG: AtomicBool = AtomicBool::new(false);
        let held = FlagGuard::acquire(&FLAG).expect("free at first");
        assert!(FlagGuard::acquire(&FLAG).is_none());
        drop(held);

        let panicked = std::panic::catch_unwind(|| {
            let _held = FlagGuard::acquire(&FLAG).expect("free after drop");
            panic!("worker failed");
        });

        assert!(panicked.is_err());
        assert!(FlagGuard::acquire(&FLAG).is_some());
    }

    const SHA_ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn verify_sha256_accepts_a_matching_digest_in_either_case() {
        assert_eq!(verify_sha256(&b"abc"[..], SHA_ABC), Ok(()));
        assert_eq!(
            verify_sha256(&b"abc"[..], &SHA_ABC.to_ascii_uppercase()),
            Ok(())
        );
    }

    #[test]
    fn verify_sha256_hashes_the_whole_stream() {
        let data = vec![7u8; 300_000];
        let expected: String = Sha256::digest(&data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();

        assert_eq!(verify_sha256(&data[..], &expected), Ok(()));
        assert_eq!(
            verify_sha256(&data[..data.len() - 1], &expected),
            Err(UpdateError::DigestMismatch)
        );
    }

    #[test]
    fn verify_sha256_rejects_a_different_file() {
        assert_eq!(
            verify_sha256(&b"abd"[..], SHA_ABC),
            Err(UpdateError::DigestMismatch)
        );
        assert_eq!(
            verify_sha256(&b""[..], SHA_ABC),
            Err(UpdateError::DigestMismatch)
        );
    }

    #[test]
    fn verify_sha256_rejects_a_malformed_expected_digest() {
        for expected in [
            "",
            "abc",
            &format!("sha256:{SHA_ABC}"),
            &SHA_ABC[..63],
            &format!("{SHA_ABC}0"),
            &format!("{}g", &SHA_ABC[..63]),
        ] {
            assert_eq!(
                verify_sha256(&b"abc"[..], expected),
                Err(UpdateError::InvalidDigest(expected.to_string())),
                "{expected:?}"
            );
        }
    }

    #[test]
    fn verify_sha256_reports_a_failing_reader() {
        struct Failing;
        impl Read for Failing {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("disk gone"))
            }
        }

        assert!(matches!(
            verify_sha256(Failing, SHA_ABC),
            Err(UpdateError::ReadFile(e)) if e.contains("disk gone")
        ));
    }

    #[test]
    fn only_signature_failures_count_as_a_signature_refusal() {
        for e in [
            UpdateError::MissingSignature,
            UpdateError::InvalidSignature("x".into()),
            UpdateError::SignatureMismatch,
            UpdateError::SignatureForOtherFile,
        ] {
            assert!(e.is_signature_refusal(), "{e:?}");
        }
        for e in [
            UpdateError::DigestMismatch,
            UpdateError::Network("x".into()),
            UpdateError::NoMatchingAsset,
        ] {
            assert!(!e.is_signature_refusal(), "{e:?}");
        }
    }

    #[test]
    fn signature_refusal_notice_names_the_version_when_known() {
        let v = Version::parse("1.2.3").unwrap();

        assert!(signature_refusal_notice(Some(v)).contains("v1.2.3"));
        assert!(signature_refusal_notice(None).contains("not installed"));
    }

    #[test]
    fn verify_signature_accepts_a_file_signed_with_the_key() {
        assert_eq!(
            verify_signature(SIGNED_CONTENT, SIGNATURE, &[PUBLIC], INSTALLER),
            Ok(())
        );
    }

    #[test]
    fn verify_signature_rejects_a_tampered_file() {
        let mut tampered = SIGNED_CONTENT.to_vec();
        tampered[0] ^= 1;

        assert_eq!(
            verify_signature(&tampered[..], SIGNATURE, &[PUBLIC], INSTALLER),
            Err(UpdateError::SignatureMismatch)
        );
        assert_eq!(
            verify_signature(&SIGNED_CONTENT[1..], SIGNATURE, &[PUBLIC], INSTALLER),
            Err(UpdateError::SignatureMismatch)
        );
    }

    #[test]
    fn verify_signature_rejects_another_key() {
        assert_eq!(
            verify_signature(SIGNED_CONTENT, SIGNATURE, &[OTHER_PUBLIC], INSTALLER),
            Err(UpdateError::SignatureMismatch)
        );
    }

    #[test]
    fn verify_signature_rejects_a_missing_or_malformed_signature() {
        for signature in ["", "not a signature", "untrusted comment: x\n"] {
            assert!(
                matches!(
                    verify_signature(SIGNED_CONTENT, signature, &[PUBLIC], INSTALLER),
                    Err(UpdateError::InvalidSignature(_))
                ),
                "{signature:?}"
            );
        }
    }

    #[test]
    fn verify_signature_reports_an_invalid_signature_when_no_key_parses() {
        let placeholder = ["REPLACE_WITH_MINISIGN_PUBLIC_KEY", ""];

        assert!(matches!(
            verify_signature(SIGNED_CONTENT, SIGNATURE, &placeholder, INSTALLER),
            Err(UpdateError::InvalidSignature(e)) if e.contains("no usable public key")
        ));
    }

    #[test]
    fn verify_signature_reports_a_mismatch_when_one_key_parses_and_another_does_not() {
        let keys = [OTHER_PUBLIC, "mistyped"];

        assert_eq!(
            verify_signature(SIGNED_CONTENT, SIGNATURE, &keys, INSTALLER),
            Err(UpdateError::SignatureMismatch)
        );
    }

    #[test]
    fn verify_signature_refuses_a_genuine_signature_made_for_another_release() {
        // Valid for the content and the key, but its trusted comment names 1.1.0:
        // an old signed installer re-published under a newer name.
        assert_eq!(
            verify_signature(SIGNED_CONTENT, SIGNATURE_FOR_1_1_0, &[PUBLIC], INSTALLER),
            Err(UpdateError::SignatureForOtherFile)
        );
    }

    #[test]
    fn verify_signature_accepts_the_next_key_next_to_the_current_one() {
        let keys = [PUBLIC, "", NEXT_PUBLIC];

        assert_eq!(
            verify_signature(SIGNED_CONTENT, SIGNATURE, &keys, INSTALLER),
            Ok(())
        );
        assert_eq!(
            verify_signature(SIGNED_CONTENT, SIGNATURE_BY_NEXT_KEY, &keys, INSTALLER),
            Ok(())
        );
        assert_eq!(
            verify_signature(SIGNED_CONTENT, SIGNATURE_BY_NEXT_KEY, &[PUBLIC], INSTALLER),
            Err(UpdateError::SignatureMismatch)
        );
    }

    #[test]
    fn verify_signature_reports_a_failing_reader() {
        struct Failing;
        impl Read for Failing {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("disk gone"))
            }
        }

        assert!(matches!(
            verify_signature(Failing, SIGNATURE, &[PUBLIC], INSTALLER),
            Err(UpdateError::ReadFile(e)) if e.contains("disk gone")
        ));
    }

    #[test]
    fn select_signature_asset_finds_the_minisig_of_the_installer() {
        let release = parse_release(FULL_RELEASE).unwrap();

        assert_eq!(
            select_signature_asset(&release).unwrap(),
            "https://github.com/ranarn/gameframe/releases/download/v1.2.0/GameFrame-Setup-1.2.0.exe.minisig"
        );
    }

    #[test]
    fn select_signature_asset_fails_without_a_signature_for_this_installer() {
        let assets = [
            asset_json(
                "GameFrame-Setup-1.2.0.exe",
                Some(&format!("sha256:{SHA_A}")),
            ),
            asset_json("GameFrame-Setup-1.1.0.exe.minisig", None),
        ]
        .join(",");
        let release = parse_release(&release_json("v1.2.0", &assets)).unwrap();

        assert_eq!(
            select_signature_asset(&release),
            Err(UpdateError::MissingSignature)
        );
    }

    #[test]
    fn select_signature_asset_rejects_an_untrusted_download_url() {
        let json = r#"{"tag_name": "v1.2.0", "assets": [{"name": "GameFrame-Setup-1.2.0.exe.minisig",
            "browser_download_url": "https://evil.test/a.minisig"}]}"#;

        assert!(matches!(
            select_signature_asset(&parse_release(json).unwrap()),
            Err(UpdateError::UntrustedDownloadUrl(_))
        ));
    }

    #[test]
    fn check_refuses_a_release_whose_installer_is_unsigned() {
        let json = release_json(
            "v1.2.0",
            &asset_json(
                "GameFrame-Setup-1.2.0.exe",
                Some(&format!("sha256:{SHA_A}")),
            ),
        );

        assert_eq!(
            check_json("1.1.0", &json),
            Err(UpdateError::MissingSignature)
        );
    }

    fn available() -> CheckOutcome {
        CheckOutcome::UpdateAvailable(UpdateInfo {
            version: Version(1, 2, 0),
            download_url: "https://github.com/a.exe".into(),
            sha256: SHA_A.into(),
            signature_url: "https://github.com/a.exe.minisig".into(),
        })
    }

    #[test]
    fn startup_action_installs_an_available_update_in_auto_mode() {
        let CheckOutcome::UpdateAvailable(info) = available() else {
            unreachable!()
        };

        assert_eq!(
            startup_action(UpdateMode::Auto, false, Ok(available())),
            StartupAction::InstallNow(info)
        );
    }

    #[test]
    fn startup_action_never_installs_when_elevated() {
        let CheckOutcome::UpdateAvailable(info) = available() else {
            unreachable!()
        };

        for mode in [UpdateMode::Auto, UpdateMode::Ask] {
            assert_eq!(
                startup_action(mode, true, Ok(available())),
                StartupAction::ReportElevated(info.clone())
            );
        }
        assert_eq!(
            startup_action(UpdateMode::Auto, true, Ok(CheckOutcome::UpToDate)),
            StartupAction::Nothing
        );
    }

    #[test]
    fn ensure_unelevated_refuses_only_an_elevated_instance() {
        assert_eq!(ensure_unelevated(false), Ok(()));
        assert_eq!(ensure_unelevated(true), Err(UpdateError::ElevatedInstance));
        assert!(
            UpdateError::ElevatedInstance
                .to_string()
                .contains("Restart GameFrame normally")
        );
    }

    #[test]
    fn startup_action_offers_an_available_update_in_ask_mode() {
        let CheckOutcome::UpdateAvailable(info) = available() else {
            unreachable!()
        };

        assert_eq!(
            startup_action(UpdateMode::Ask, false, Ok(available())),
            StartupAction::OfferInTray(info)
        );
    }

    #[test]
    fn startup_action_does_nothing_without_an_update_or_on_an_error() {
        for mode in [UpdateMode::Auto, UpdateMode::Ask] {
            for result in [
                Ok(CheckOutcome::UpToDate),
                Ok(CheckOutcome::DevelopmentBuild),
                Ok(CheckOutcome::NoReleasePublished),
                Err(UpdateError::Network("offline".into())),
                Err(UpdateError::HttpStatus(403)),
                Err(UpdateError::Malformed("x".into())),
            ] {
                assert_eq!(startup_action(mode, false, result), StartupAction::Nothing);
            }
        }
    }

    #[test]
    fn result_message_describes_every_outcome() {
        let info = UpdateInfo {
            version: Version(1, 2, 0),
            download_url: "https://github.com/a.exe".into(),
            sha256: SHA_A.into(),
            signature_url: "https://github.com/a.exe.minisig".into(),
        };

        assert_eq!(
            result_message(&Ok(CheckOutcome::UpToDate)),
            "GameFrame is up to date."
        );
        assert_eq!(
            result_message(&Ok(CheckOutcome::UpdateAvailable(info))),
            "Update v1.2.0 is available."
        );
        assert!(result_message(&Ok(CheckOutcome::DevelopmentBuild)).contains("development build"));
        assert!(result_message(&Ok(CheckOutcome::NoReleasePublished)).contains("published"));
        assert!(
            result_message(&Err(UpdateError::Network("timed out".into()))).contains("timed out")
        );
    }
}
