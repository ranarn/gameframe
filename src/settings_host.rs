//! The tray process's end of the link to a Settings / zone editor child, see
//! [`crate::settings_link`] for why the windows live in a child process.
//!
//! OS-facing (spawns a process, timers, thread messages): checked manually. The
//! wire format and the child's end are tested in [`crate::settings_link`].

use crate::cursor_style::Bitmap;
use crate::hotkeys::BindingReport;
use crate::instance_channel::{self, SettingsGuard};
use crate::settings_link::{ChildKind, ChildMsg, ParentMsg, child_command_line, decode_line};
use crate::settings_ui::TickContext;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, KillTimer, PostThreadMessageW, SetTimer, WM_NULL,
};

/// `CREATE_NO_WINDOW`: a debug build is a console program, and the child must
/// not open a console of its own.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// While a child runs, the main loop wakes this often even if no message
/// arrives, to notice the child's messages and exit (a modal loop on the main
/// thread can swallow the one-off wake-up the reader thread posts).
const POLL_MS: u32 = 250;

/// How long [`SettingsProcess::close`] lets the child close its window (it
/// restores the taskbar, among other things) before ending it.
const CLOSE_GRACE: Duration = Duration::from_secs(3);

/// After the child exited: how long to wait for the messages it sent last.
const EXIT_DRAIN: Duration = Duration::from_millis(200);

enum Event {
    Msg(ChildMsg),
    /// The child closed its stdout.
    Eof,
}

/// A running child: the Settings window or the first-run zone editor.
pub struct SettingsProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    events: Receiver<Event>,
    reader_done: bool,
    exited: bool,
    /// Which window is ticking now, as the child last reported; decides which
    /// hotkeys still run.
    pub context: TickContext,
    timer: usize,
    /// Marks Settings as open for the instance channel (Settings only).
    _modal: Option<SettingsGuard>,
}

/// What [`SettingsProcess::poll`] found.
pub struct Polled {
    /// The child's messages since the last poll, in order.
    pub messages: Vec<ChildMsg>,
    /// The child has exited (its last messages are in `messages`).
    pub exited: bool,
    /// Its exit code, when it exited and the code is known.
    pub exit_code: Option<i32>,
}

impl SettingsProcess {
    /// Starts a linked child of `kind` and sends it what it cannot find out
    /// itself. Call on the main thread (the reader thread wakes that thread).
    pub fn spawn(
        kind: ChildKind,
        hotkey_reports: Vec<BindingReport>,
        game_cursor: Option<Bitmap>,
    ) -> std::io::Result<Self> {
        let exe = std::env::current_exe()?;
        let mut child = Command::new(exe)
            .args(child_command_line(kind))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()?;
        let stdout = child.stdout.take();
        let stdin = child.stdin.take();
        let pid = child.id();
        // Lets the child's window come to the front.
        // SAFETY: plain Win32 call with a process id.
        unsafe { AllowSetForegroundWindow(pid) };

        // SAFETY: no arguments or preconditions.
        let main_thread = unsafe { GetCurrentThreadId() };
        let (tx, events) = channel();
        let reader = std::thread::Builder::new()
            .name("settings-link".into())
            .spawn(move || {
                let Some(stdout) = stdout else { return };
                let wake = || {
                    // SAFETY: plain post to a thread id; fails harmlessly if gone.
                    unsafe { PostThreadMessageW(main_thread, WM_NULL, 0, 0) };
                };
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    if let Some(msg) = decode_line::<ChildMsg>(&line)
                        && tx.send(Event::Msg(msg)).is_ok()
                    {
                        wake();
                    }
                }
                let _ = tx.send(Event::Eof);
                wake();
            });
        if let Err(e) = reader {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }

        // SAFETY: a thread timer (no window, no callback) on this thread.
        let timer = unsafe { SetTimer(std::ptr::null_mut(), 0, POLL_MS, None) };
        let (context, modal) = match kind {
            ChildKind::Settings => (
                TickContext::Settings,
                Some(instance_channel::enter_settings(pid)),
            ),
            ChildKind::ZoneEditor => (TickContext::ZoneEditor, None),
        };
        let mut process = Self {
            child,
            stdin,
            events,
            reader_done: false,
            exited: false,
            context,
            timer,
            _modal: modal,
        };
        process.send(&ParentMsg::Init {
            hotkey_reports,
            game_cursor,
        });
        Ok(process)
    }

    /// Sends `msg`; a failure (the child is gone) is dropped, `poll` reports
    /// the exit.
    pub fn send(&mut self, msg: &ParentMsg) {
        let Some(stdin) = self.stdin.as_mut() else {
            return;
        };
        let line = crate::settings_link::encode_line(msg);
        if stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.flush())
            .is_err()
        {
            self.stdin = None;
        }
    }

    /// Collects the child's messages and whether it has exited.
    pub fn poll(&mut self) -> Polled {
        let mut messages = Vec::new();
        self.drain(&mut messages);
        let mut exit_code = None;
        if !self.exited {
            match self.child.try_wait() {
                Ok(None) => {}
                Ok(Some(status)) => {
                    self.exited = true;
                    exit_code = status.code();
                }
                Err(e) => {
                    log!("couldn't check the settings process: {e}");
                    self.exited = true;
                }
            }
            if self.exited {
                // Its last messages may still be on the way through the pipe.
                let deadline = Instant::now() + EXIT_DRAIN;
                while !self.reader_done {
                    let left = deadline.saturating_duration_since(Instant::now());
                    match self.events.recv_timeout(left) {
                        Ok(Event::Msg(msg)) => messages.push(msg),
                        Ok(Event::Eof) => self.reader_done = true,
                        Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
                    }
                }
            }
        }
        Polled {
            messages,
            exited: self.exited,
            exit_code,
        }
    }

    fn drain(&mut self, messages: &mut Vec<ChildMsg>) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                Event::Msg(msg) => messages.push(msg),
                Event::Eof => self.reader_done = true,
            }
        }
    }

    /// Asks the child to close and waits [`CLOSE_GRACE`] for it to exit, then
    /// ends it. Unsaved edits are discarded, as with the window's close button.
    pub fn close(&mut self) {
        if self.exited {
            return;
        }
        self.send(&ParentMsg::Close);
        // Also EOF for the child, should it have missed the message.
        self.stdin = None;
        let deadline = Instant::now() + CLOSE_GRACE;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => {
                    self.exited = true;
                    return;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        log!("the settings process did not close in time; ending it");
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.exited = true;
    }
}

impl Drop for SettingsProcess {
    fn drop(&mut self) {
        self.close();
        // SAFETY: the id `SetTimer` returned for this thread's timer (a failed
        // call's 0 makes this a harmless no-op).
        unsafe { KillTimer(std::ptr::null_mut(), self.timer) };
    }
}
