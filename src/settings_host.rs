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
use std::process::{Child, Command, Stdio};
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

/// What [`SettingsProcess`] needs from the child process: `Child`, or a fake in
/// the tests.
pub trait ProcessHandle {
    fn id(&self) -> u32;
    /// `Ok(None)` while it runs; `Ok(Some(code))` once it has exited, with its
    /// exit code when known.
    fn try_exit(&mut self) -> std::io::Result<Option<Option<i32>>>;
    /// Ends the process and reaps it.
    fn kill(&mut self);
}

impl ProcessHandle for Child {
    fn id(&self) -> u32 {
        Child::id(self)
    }

    fn try_exit(&mut self) -> std::io::Result<Option<Option<i32>>> {
        self.try_wait().map(|status| status.map(|s| s.code()))
    }

    fn kill(&mut self) {
        let _ = Child::kill(self);
        let _ = self.wait();
    }
}

/// After the child exited: how long to wait for the messages it sent last.
const EXIT_DRAIN: Duration = Duration::from_millis(200);

enum Event {
    Msg(ChildMsg),
    /// The child closed its stdout.
    Eof,
}

/// A running child: the Settings window or the first-run zone editor.
pub struct SettingsProcess<C: ProcessHandle = Child> {
    child: C,
    stdin: Option<Box<dyn Write + Send>>,
    events: Receiver<Event>,
    reader_done: bool,
    exited: bool,
    close_grace: Duration,
    /// Runs once, with the child's id, when it has ended however it ended
    /// (exit, kill, crash): the tray process undoes what a child killed
    /// mid-way may have left behind (the taskbar).
    on_ended: Option<Box<dyn FnOnce(u32)>>,
    /// Which window is ticking now, as the child last reported; decides which
    /// hotkeys still run.
    pub context: TickContext,
    timer: Option<usize>,
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

impl SettingsProcess<Child> {
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
        let stdin = child
            .stdin
            .take()
            .map(|s| Box::new(s) as Box<dyn Write + Send>);
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
        let mut process = Self::from_parts(child, stdin, events, context, modal);
        process.timer = Some(timer);
        process.on_ended = Some(Box::new(|pid| {
            crate::taskbar_recovery::recover_child(&crate::app::config_dir(), pid);
        }));
        process.send(&ParentMsg::Init {
            hotkey_reports,
            game_cursor,
        });
        Ok(process)
    }
}

impl<C: ProcessHandle> SettingsProcess<C> {
    fn from_parts(
        child: C,
        stdin: Option<Box<dyn Write + Send>>,
        events: Receiver<Event>,
        context: TickContext,
        modal: Option<SettingsGuard>,
    ) -> Self {
        Self {
            child,
            stdin,
            events,
            reader_done: false,
            exited: false,
            close_grace: CLOSE_GRACE,
            on_ended: None,
            context,
            timer: None,
            _modal: modal,
        }
    }

    /// Records that the child has ended and runs the end hook (once).
    fn end(&mut self) {
        self.exited = true;
        if let Some(on_ended) = self.on_ended.take() {
            on_ended(self.child.id());
        }
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
            match self.child.try_exit() {
                Ok(None) => {}
                Ok(Some(code)) => {
                    self.end();
                    exit_code = code;
                }
                Err(e) => {
                    // Not known to be gone: end it, or it would run on orphaned
                    // while the tray process undoes its effects underneath it.
                    log!("couldn't check the settings process: {e}; ending it");
                    self.child.kill();
                    self.end();
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
        let deadline = Instant::now() + self.close_grace;
        while Instant::now() < deadline {
            match self.child.try_exit() {
                Ok(Some(_)) => {
                    self.end();
                    return;
                }
                Err(_) => break,
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        log!("the settings process did not close in time; ending it");
        self.child.kill();
        self.end();
    }
}

impl<C: ProcessHandle> Drop for SettingsProcess<C> {
    fn drop(&mut self) {
        self.close();
        if let Some(timer) = self.timer {
            // SAFETY: the id `SetTimer` returned for this thread's timer (a
            // failed call's 0 makes this a harmless no-op).
            unsafe { KillTimer(std::ptr::null_mut(), timer) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::sync::mpsc::Sender;

    /// A child that exits when told to (or never).
    struct FakeChild {
        exit_code: Rc<Cell<Option<i32>>>,
        killed: Rc<Cell<bool>>,
        /// Makes the status check itself fail.
        check_fails: Rc<Cell<bool>>,
    }

    impl ProcessHandle for FakeChild {
        fn id(&self) -> u32 {
            4711
        }
        fn try_exit(&mut self) -> std::io::Result<Option<Option<i32>>> {
            if self.check_fails.get() {
                return Err(std::io::Error::other("no status"));
            }
            Ok(self.exit_code.get().map(Some))
        }
        fn kill(&mut self) {
            self.killed.set(true);
        }
    }

    /// A stdin that records what the tray process sent.
    #[derive(Clone, Default)]
    struct Sent(Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for Sent {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    use std::sync::Arc;

    struct Rig {
        process: SettingsProcess<FakeChild>,
        exit_code: Rc<Cell<Option<i32>>>,
        killed: Rc<Cell<bool>>,
        check_fails: Rc<Cell<bool>>,
        to_tray: Sender<Event>,
        sent: Sent,
        ended: Rc<RefCell<Vec<u32>>>,
    }

    fn rig() -> Rig {
        let exit_code = Rc::new(Cell::new(None));
        let killed = Rc::new(Cell::new(false));
        let check_fails = Rc::new(Cell::new(false));
        let (to_tray, events) = channel();
        let sent = Sent::default();
        let ended = Rc::new(RefCell::new(Vec::new()));
        let mut process = SettingsProcess::from_parts(
            FakeChild {
                exit_code: Rc::clone(&exit_code),
                killed: Rc::clone(&killed),
                check_fails: Rc::clone(&check_fails),
            },
            Some(Box::new(sent.clone())),
            events,
            TickContext::Settings,
            None,
        );
        process.close_grace = Duration::from_millis(60);
        let log = Rc::clone(&ended);
        process.on_ended = Some(Box::new(move |pid| log.borrow_mut().push(pid)));
        Rig {
            process,
            exit_code,
            killed,
            check_fails,
            to_tray,
            sent,
            ended,
        }
    }

    #[test]
    fn a_running_child_hands_over_its_messages_in_order_and_has_not_exited() {
        let mut rig = rig();
        rig.to_tray
            .send(Event::Msg(ChildMsg::ConfigChanged))
            .unwrap();
        rig.to_tray.send(Event::Msg(ChildMsg::ReapplyAll)).unwrap();

        let polled = rig.process.poll();

        assert_eq!(
            polled.messages,
            vec![ChildMsg::ConfigChanged, ChildMsg::ReapplyAll]
        );
        assert!(!polled.exited);
        assert!(rig.ended.borrow().is_empty());
    }

    #[test]
    fn the_exit_is_reported_once_with_its_code_and_runs_the_end_hook() {
        let mut rig = rig();
        rig.exit_code.set(Some(10));
        rig.to_tray.send(Event::Eof).unwrap();

        let polled = rig.process.poll();

        assert!(polled.exited);
        assert_eq!(polled.exit_code, Some(10));
        assert_eq!(*rig.ended.borrow(), vec![4711]);
        let again = rig.process.poll();
        assert!(again.exited && again.exit_code.is_none());
        assert_eq!(rig.ended.borrow().len(), 1, "the hook runs once");
    }

    #[test]
    fn messages_still_in_the_pipe_when_the_child_exits_are_not_lost() {
        let mut rig = rig();
        rig.exit_code.set(Some(0));
        let late = rig.to_tray.clone();
        let sender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            late.send(Event::Msg(ChildMsg::ConfigChanged)).unwrap();
            late.send(Event::Eof).unwrap();
        });

        let polled = rig.process.poll();
        sender.join().unwrap();

        assert!(polled.exited);
        assert_eq!(polled.messages, vec![ChildMsg::ConfigChanged]);
    }

    #[test]
    fn closing_asks_the_child_to_close_first_and_does_not_kill_one_that_exits_in_time() {
        let mut rig = rig();
        // The fake exits at once, as a child that got the request in time.
        rig.exit_code.set(Some(0));

        rig.process.close();

        assert!(!rig.killed.get());
        assert_eq!(*rig.ended.borrow(), vec![4711]);
        let sent = String::from_utf8(rig.sent.0.lock().unwrap().clone()).unwrap();
        assert!(
            sent.contains("Close"),
            "the child is asked to close first: {sent:?}"
        );
    }

    #[test]
    fn a_child_that_ignores_the_close_request_is_killed_after_the_grace_period() {
        let mut rig = rig();

        rig.process.close();

        assert!(rig.killed.get());
        assert_eq!(
            *rig.ended.borrow(),
            vec![4711],
            "its leftovers are still recovered"
        );
        assert!(rig.process.poll().exited);
    }

    #[test]
    fn dropping_the_process_closes_it_and_runs_the_end_hook() {
        let rig = rig();
        let killed = Rc::clone(&rig.killed);
        let ended = Rc::clone(&rig.ended);

        drop(rig.process);

        assert!(killed.get());
        assert_eq!(*ended.borrow(), vec![4711]);
    }

    #[test]
    fn a_child_whose_status_cannot_be_read_is_ended_so_it_cannot_run_on_orphaned() {
        let mut rig = rig();
        rig.check_fails.set(true);

        let polled = rig.process.poll();

        assert!(polled.exited);
        assert!(rig.killed.get());
        assert_eq!(*rig.ended.borrow(), vec![4711]);
    }

    #[test]
    fn closing_an_exited_child_does_nothing_more() {
        let mut rig = rig();
        rig.exit_code.set(Some(0));
        rig.process.poll();
        rig.process.close();

        assert!(!rig.killed.get());
        assert_eq!(rig.ended.borrow().len(), 1);
    }
}
