//! The Updates section: the update check and the install it can start.

use super::SettingsApp;
use crate::settings_link::InstallProgress;
use crate::updater::{CheckOutcome, SpawnError, UpdateError, UpdateInfo};
use std::time::{Duration, Instant};

/// Progress of the Settings window's update check.
pub(super) enum UpdateCheck {
    Idle,
    /// Running on a worker thread, which sends the result here.
    Running(std::sync::mpsc::Receiver<Result<CheckOutcome, UpdateError>>),
    Done(Result<CheckOutcome, UpdateError>),
    /// Not started: another check (the tray's) was already running.
    Busy,
    /// The worker ended without sending a result.
    Lost,
}

/// Progress of installing the update an `UpdateCheck` found.
pub(super) enum UpdateInstall {
    Idle,
    /// Asked for; the tray process downloads and verifies it and reports back
    /// (see [`crate::settings_link::InstallProgress`]).
    Running,
    /// The installer was started (at this time); it asks GameFrame to quit.
    Started(Instant),
    /// The installer was started but GameFrame wasn't quit within
    /// [`INSTALL_QUIT_TIMEOUT`].
    TimedOut,
    /// Failed; the text is the message for the user.
    Failed(String),
    /// Not started: another install was already running.
    Busy,
    /// Not started: the worker thread could not be created, or there is no
    /// tray process to ask.
    NotStarted,
}

/// How long Settings waits, after the installer started, for it to quit
/// GameFrame before reporting that the installer didn't finish.
const INSTALL_QUIT_TIMEOUT: Duration = Duration::from_secs(120);

impl UpdateInstall {
    /// Applies what the tray process reported about the install that is
    /// `Running`; progress that arrives in any other state (an install the tray
    /// item started) is not this window's business.
    fn apply(&mut self, progress: InstallProgress, now: Instant) {
        if !matches!(self, Self::Running) {
            return;
        }
        *self = match progress {
            InstallProgress::Launched => Self::Started(now),
            InstallProgress::Failed(text) => Self::Failed(text),
            InstallProgress::Busy => Self::Busy,
            InstallProgress::NotStarted => Self::NotStarted,
            // Shown on the failure line (the install did not run), but as the
            // advice itself, without the "Installing the update failed:" prefix.
            InstallProgress::Elevated => {
                Self::Failed(crate::updater::ELEVATED_UPDATE_ADVICE.into())
            }
        };
    }

    /// Moves `Started` to `TimedOut` once [`INSTALL_QUIT_TIMEOUT`] has passed
    /// between its start and `now`.
    fn expire(&mut self, now: Instant) {
        if let Self::Started(since) = self
            && now.saturating_duration_since(*since) >= INSTALL_QUIT_TIMEOUT
        {
            *self = Self::TimedOut;
        }
    }
}

impl SettingsApp<'_> {
    /// Renders the "Check for updates now" button and its status line. The
    /// check runs on a worker thread that wakes the UI when it has a result.
    pub(super) fn update_check_row(&mut self, ui: &mut egui::Ui) {
        if let UpdateCheck::Running(receiver) = &self.update_check {
            match receiver.try_recv() {
                Ok(result) => self.update_check = UpdateCheck::Done(result),
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.update_check = UpdateCheck::Lost;
                }
            }
        }

        ui.horizontal(|ui| {
            // A new check while an install runs would drop the install's result.
            let installing = matches!(
                self.update_install,
                UpdateInstall::Running | UpdateInstall::Started(_)
            );
            let enabled = !installing && !matches!(self.update_check, UpdateCheck::Running(_));
            if ui
                .add_enabled(enabled, egui::Button::new("Check for updates now"))
                .clicked()
            {
                let (sender, receiver) = std::sync::mpsc::channel();
                let ctx = ui.ctx().clone();
                let started = crate::updater::spawn_check(move |result| {
                    if matches!(result, Ok(CheckOutcome::UpToDate)) {
                        crate::settings_link::report_up_to_date();
                    }
                    let _ = sender.send(result);
                    ctx.request_repaint();
                });
                self.update_check = match started {
                    Ok(()) => UpdateCheck::Running(receiver),
                    Err(SpawnError::Busy) => UpdateCheck::Busy,
                    Err(SpawnError::ThreadFailed) => UpdateCheck::Lost,
                };
                self.update_install = UpdateInstall::Idle;
            }
            match &self.update_check {
                UpdateCheck::Idle => {}
                UpdateCheck::Running(_) => {
                    ui.spinner();
                    ui.label("Checking...");
                }
                UpdateCheck::Busy => {
                    ui.label("Another update check is still running.");
                }
                UpdateCheck::Lost => {
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        "Update check failed: it ended unexpectedly",
                    );
                }
                UpdateCheck::Done(result) => {
                    let message = crate::updater::result_message(result);
                    if result.is_ok() {
                        ui.label(message);
                    } else {
                        ui.colored_label(ui.visuals().error_fg_color, message);
                    }
                }
            }
        });

        if let UpdateCheck::Done(Ok(CheckOutcome::UpdateAvailable(info))) = &self.update_check {
            let info = info.clone();
            ui.horizontal(|ui| self.update_install_row(ui, info));
        }
    }

    /// Renders the "Install update" button and its status line. The tray
    /// process runs the install and reports its progress; on success the
    /// installer quits GameFrame (closing this window) and starts it again.
    fn update_install_row(&mut self, ui: &mut egui::Ui, info: UpdateInfo) {
        self.update_install.expire(Instant::now());
        if matches!(self.update_install, UpdateInstall::Running) {
            // The progress arrives through the link, which wakes nothing.
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        } else if matches!(self.update_install, UpdateInstall::Started(_)) {
            // Nothing else wakes the UI if the installer never quits GameFrame.
            ui.ctx().request_repaint_after(Duration::from_secs(5));
        }
        for progress in crate::settings_link::take_install_progress() {
            self.update_install.apply(progress, Instant::now());
        }

        let busy = matches!(
            self.update_install,
            UpdateInstall::Running | UpdateInstall::Started(_)
        );
        let label = format!("Install v{}", info.version);
        if ui.add_enabled(!busy, egui::Button::new(label)).clicked() {
            // The tray process runs the install, so closing this window never
            // interrupts it; its progress arrives through the link. Without a
            // tray process (a standalone debug child) there is nothing to ask.
            self.update_install = if crate::settings_link::request_install(&info) {
                UpdateInstall::Running
            } else {
                UpdateInstall::NotStarted
            };
        }
        match &self.update_install {
            UpdateInstall::Idle => {}
            UpdateInstall::Running => {
                ui.spinner();
                ui.label("Downloading the update...");
            }
            UpdateInstall::Started(_) => {
                ui.label("Installing; GameFrame restarts in a moment.");
            }
            UpdateInstall::TimedOut => {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    "The installer didn't finish. Try installing again.",
                );
            }
            UpdateInstall::Failed(message) => {
                ui.colored_label(ui.visuals().error_fg_color, message);
            }
            UpdateInstall::Busy => {
                ui.label("An update is already being installed.");
            }
            UpdateInstall::NotStarted => {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    "Couldn't start the update install.",
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_started_times_out_after_the_timeout_and_not_before() {
        let start = Instant::now();
        let just_before = start + INSTALL_QUIT_TIMEOUT - Duration::from_millis(1);

        let mut state = UpdateInstall::Started(start);
        state.expire(just_before);
        assert!(matches!(state, UpdateInstall::Started(_)));

        state.expire(start + INSTALL_QUIT_TIMEOUT);
        assert!(matches!(state, UpdateInstall::TimedOut));
    }

    #[test]
    fn the_tray_processs_progress_moves_a_running_install_on() {
        let now = Instant::now();
        let progress = |p| {
            let mut state = UpdateInstall::Running;
            state.apply(p, now);
            state
        };
        assert!(matches!(
            progress(InstallProgress::Launched),
            UpdateInstall::Started(at) if at == now
        ));
        assert!(matches!(
            progress(InstallProgress::Failed("disk full".into())),
            UpdateInstall::Failed(text) if text == "disk full"
        ));
        assert!(matches!(
            progress(InstallProgress::Busy),
            UpdateInstall::Busy
        ));
        assert!(matches!(
            progress(InstallProgress::NotStarted),
            UpdateInstall::NotStarted
        ));
        assert!(matches!(
            progress(InstallProgress::Elevated),
            UpdateInstall::Failed(text) if text == crate::updater::ELEVATED_UPDATE_ADVICE
        ));
    }

    #[test]
    fn progress_of_an_install_this_window_did_not_ask_for_is_ignored() {
        // E.g. the tray item started one; Settings is idle.
        let mut state = UpdateInstall::Idle;
        state.apply(InstallProgress::Launched, Instant::now());
        assert!(matches!(state, UpdateInstall::Idle));
    }

    #[test]
    fn expire_leaves_other_install_states_alone() {
        let mut state = UpdateInstall::Idle;
        state.expire(Instant::now() + INSTALL_QUIT_TIMEOUT * 2);
        assert!(matches!(state, UpdateInstall::Idle));
    }
}
