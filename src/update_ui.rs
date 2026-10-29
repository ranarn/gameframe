//! The event loop's side of updating: the startup check, the tray's
//! "Update to vX.Y.Z" item (ask mode) and the installs, whoever starts them.
//!
//! Worker threads never touch the tray; they send an [`UpdateEvent`] through an
//! [`UpdateNotifier`] and the event loop acts on it via [`handle_update_event`].
//! OS-facing glue, checked manually (the decisions it makes are tested in
//! [`crate::updater`]).

use crate::message_box::{show_message_box, show_warning_box};
use crate::profile_store;
use crate::tray_ui::Tray;
use crate::updater::{
    CheckOutcome, InstallStartError, StartupAction, UpdateError, UpdateInfo, Version,
};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    MB_ICONINFORMATION, MB_OK, PostThreadMessageW, WM_NULL,
};

/// How long after startup the update check runs, so startup isn't slowed.
const STARTUP_UPDATE_CHECK_DELAY: Duration = Duration::from_secs(20);

/// How long a started installer has to quit this instance before GameFrame
/// assumes it failed and restores the tray.
const INSTALLER_QUIT_TIMEOUT: Duration = Duration::from_secs(120);

/// The event loop's notifier, for events raised outside this module (the
/// "Check for Updates Now" box). Set once by [`UpdateEvents::new`].
static NOTIFIER: OnceLock<UpdateNotifier> = OnceLock::new();

/// What a worker thread reports to the event loop about updating.
pub(crate) enum UpdateEvent {
    /// The startup check finished.
    Checked(Result<CheckOutcome, UpdateError>),
    /// A manual check found no newer release.
    UpToDate,
    /// An install started: it downloads this version.
    InstallStarted(Version),
    /// The installer of this version was started; it should quit GameFrame.
    InstallerLaunched(Version),
    /// An install failed, or the installer never quit this instance.
    InstallFailed,
    /// An automatic install of this version was refused for its signature.
    SignatureRefused(Version),
    /// An install [`Failure::Forward`]ed to an open Settings window failed;
    /// the text is for the user. The tray's own handling is [`Self::InstallFailed`].
    InstallError(String),
    /// An install [`Failure::Forward`]ed to Settings could not start.
    InstallNotStarted(InstallStartError),
}

/// Lets worker threads hand an [`UpdateEvent`] to the event loop and wake its
/// `GetMessageW` with a no-op thread message.
#[derive(Clone)]
pub(crate) struct UpdateNotifier {
    sender: std::sync::mpsc::Sender<UpdateEvent>,
    loop_thread: u32,
}

impl UpdateNotifier {
    /// Queues `event` and wakes the loop. A failed post just delays the event
    /// to the next message (a modal loop on the loop thread, such as a menu,
    /// can consume the wake-up). That is harmless for a startup check.
    fn notify(&self, event: UpdateEvent) {
        if self.sender.send(event).is_ok() {
            // SAFETY: plain post to the stored thread id; fails harmlessly if it is gone.
            unsafe { PostThreadMessageW(self.loop_thread, WM_NULL, 0, 0) };
        }
    }
}

/// Sends `event` to the event loop; does nothing before the loop exists.
pub(crate) fn notify(event: UpdateEvent) {
    if let Some(notifier) = NOTIFIER.get() {
        notifier.notify(event);
    }
}

/// The event loop's end of the update channel; created on the loop's thread.
pub(crate) struct UpdateEvents {
    notifier: UpdateNotifier,
    receiver: std::sync::mpsc::Receiver<UpdateEvent>,
}

impl UpdateEvents {
    pub(crate) fn new() -> Self {
        let (sender, receiver) = std::sync::mpsc::channel();
        // SAFETY: no arguments or preconditions.
        let loop_thread = unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() };
        let notifier = UpdateNotifier {
            sender,
            loop_thread,
        };
        let _ = NOTIFIER.set(notifier.clone());
        Self { notifier, receiver }
    }

    pub(crate) fn notifier(&self) -> UpdateNotifier {
        self.notifier.clone()
    }

    pub(crate) fn try_recv(&self) -> Option<UpdateEvent> {
        self.receiver.try_recv().ok()
    }
}

/// Waits [`STARTUP_UPDATE_CHECK_DELAY`] on its own thread, then runs the
/// update check (a development build fetches nothing, see
/// [`crate::updater::check`]) and reports to the event loop. A failed check
/// is only logged, by `spawn_check`, with no retry; except a refused signature,
/// which the event loop shows as a tray balloon.
pub(crate) fn schedule_startup_update_check(notifier: UpdateNotifier) {
    let spawned = std::thread::Builder::new()
        .name("update-startup".into())
        .spawn(move || {
            std::thread::sleep(STARTUP_UPDATE_CHECK_DELAY);
            let started = crate::updater::spawn_check(move |result| {
                notifier.notify(UpdateEvent::Checked(result));
            });
            if let Err(why) = started {
                log!("startup update check not started: {why:?}");
            }
        });
    if let Err(e) = spawned {
        log!("couldn't start the startup update check: {e}");
    }
}

/// Acts on an [`UpdateEvent`] on the event loop's thread.
pub(crate) fn handle_update_event(
    event: UpdateEvent,
    tray: &Tray,
    config_dir: &Path,
    offered_update: &mut Option<UpdateInfo>,
) {
    match event {
        UpdateEvent::Checked(result) => {
            if let Err(e) = &result
                && e.is_signature_refusal()
            {
                log!("startup update check: refused: {e}");
                tray.notify(&crate::updater::signature_refusal_notice(None));
            }
            // Read now, not at startup, so a mode saved in Settings in the
            // meantime counts. Without a readable mode nothing is done: an
            // install the user may have declined is worse than a missed one.
            let mode = match profile_store::load(config_dir) {
                Ok(config) => config.update_mode,
                Err(e) => {
                    log!("startup update check: couldn't read the update mode: {e}");
                    return;
                }
            };
            let elevated = crate::win32::is_process_elevated();
            match crate::updater::startup_action(mode, elevated, result) {
                StartupAction::Nothing => {}
                StartupAction::ReportElevated(info) => {
                    log!(
                        "startup update check: v{} is available but this instance is elevated; not installing",
                        info.version
                    );
                    let text = format!(
                        "GameFrame v{} is available.\n\n{}",
                        info.version,
                        crate::updater::ELEVATED_UPDATE_ADVICE
                    );
                    // A box blocks, and this is the event loop's thread.
                    std::thread::spawn(move || {
                        show_message_box(&text, MB_OK | MB_ICONINFORMATION);
                    });
                }
                StartupAction::InstallNow(info) => {
                    log!("startup update check: installing v{}", info.version);
                    start_install(info, Failure::LogOnly);
                }
                StartupAction::OfferInTray(info) => {
                    log!("startup update check: offering v{}", info.version);
                    tray.show_update_item(&info.version.to_string());
                    *offered_update = Some(info);
                }
            }
        }
        UpdateEvent::UpToDate => {
            *offered_update = None;
            tray.hide_update_item();
        }
        UpdateEvent::InstallStarted(version) => {
            // Whoever started the install, the tray item can't start another.
            tray.hide_update_item();
            tray.set_tooltip(&format!("GameFrame - downloading update v{version}..."));
        }
        UpdateEvent::InstallerLaunched(version) => {
            tray.set_tooltip(&format!("GameFrame - installing update v{version}..."));
        }
        UpdateEvent::SignatureRefused(version) => {
            tray.notify(&crate::updater::signature_refusal_notice(Some(version)));
        }
        UpdateEvent::InstallFailed => {
            tray.set_tooltip(&crate::version::current_tooltip());
            // Ask mode: offer the update again.
            if let Some(info) = offered_update {
                tray.show_update_item(&info.version.to_string());
            }
        }
        // For the Settings window, see `crate::app`.
        UpdateEvent::InstallError(_) | UpdateEvent::InstallNotStarted(_) => {}
    }
}

/// What to do when an install started by [`start_install`] fails.
#[derive(Clone, Copy)]
pub(crate) enum Failure {
    /// Tell the user in a box: they chose the install.
    Report,
    /// Only log: the automatic startup install.
    LogOnly,
    /// Settings asked for it (#186): the failure, or a refusal to start, goes
    /// back to its window as an [`UpdateEvent`], and to a box if it is closed.
    Forward,
}

/// Starts the install of `info` on a worker thread, see [`spawn_install`].
/// `failure` says whether the user hears about a failure.
pub(crate) fn start_install(info: UpdateInfo, failure: Failure) {
    let version = info.version;
    let started = spawn_install(info, move |result| match (result, failure) {
        (Err(e), Failure::Report) => {
            show_warning_box(&crate::update_install::install_error_message(&e));
        }
        // Nobody asked for this install, but a refused signature is never
        // left to the log.
        (Err(e), Failure::LogOnly) if e.is_signature_refusal() => {
            log!("automatic update to v{version} refused: {e}");
            notify(UpdateEvent::SignatureRefused(version));
        }
        (Err(e), Failure::Forward) => {
            notify(UpdateEvent::InstallError(
                crate::update_install::install_error_message(&e),
            ));
        }
        _ => {}
    });
    if let Err(why) = started {
        log!("update install not started: {why:?}");
        match failure {
            Failure::Report => show_not_started(why),
            Failure::Forward => notify(UpdateEvent::InstallNotStarted(why)),
            Failure::LogOnly => {}
        }
    }
}

/// Tells the user why an install didn't start, from a thread of its own: the
/// box blocks, and the caller may be the event loop.
pub(crate) fn show_not_started(why: InstallStartError) {
    let text = match why {
        InstallStartError::Busy => "An update is already being installed.",
        InstallStartError::ThreadFailed => "Couldn't start the update install.",
        InstallStartError::Elevated => crate::updater::ELEVATED_UPDATE_ADVICE,
    };
    std::thread::spawn(move || {
        show_message_box(text, MB_OK | MB_ICONINFORMATION);
    });
}

/// Installs `info` on a worker thread (see
/// [`crate::update_install::spawn_install`]) and keeps the tray in step with
/// it, whoever started it: the tooltip shows the progress, the "Update to ..."
/// item is hidden meanwhile, and both are restored after a failure. On success
/// the installer quits GameFrame; if it hasn't within
/// [`INSTALLER_QUIT_TIMEOUT`] the tray is restored as after a failure. The
/// install stays "in flight" for that whole wait, so no second one starts
/// while the first installer may still be running. `on_done` gets the result
/// on the worker thread.
fn spawn_install(
    info: UpdateInfo,
    on_done: impl FnOnce(Result<(), UpdateError>) + Send + 'static,
) -> Result<(), InstallStartError> {
    let version = info.version;
    crate::update_install::spawn_install(
        info,
        move || notify(UpdateEvent::InstallStarted(version)),
        move |result| match result {
            Ok(()) => {
                on_done(Ok(()));
                notify(UpdateEvent::InstallerLaunched(version));
                // Normally the installer quits this process long before this.
                std::thread::sleep(INSTALLER_QUIT_TIMEOUT);
                log!("update install: the installer didn't quit GameFrame; carrying on");
                notify(UpdateEvent::InstallFailed);
            }
            Err(e) => {
                notify(UpdateEvent::InstallFailed);
                on_done(Err(e));
            }
        },
    )
}
