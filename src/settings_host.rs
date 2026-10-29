//! The tray process's end of the link to a Settings / zone editor child, see
//! [`crate::settings_link`] for why the windows live in a child process.
//!
//! Also handles the child's messages on the tray side (`service_settings`,
//! `react_to_child_message`) and routes install progress to the child.
//!
//! Spawning the process, its timers and thread messages are OS-facing and
//! checked manually; the process lifecycle (behind `ProcessHandle`), the
//! message handling and the install routing are unit tested here. The wire
//! format and the child's end are tested in [`crate::settings_link`].

use crate::cursor_style::Bitmap;
use crate::hotkeys::BindingReport;
use crate::instance_channel::{self, SettingsGuard};
use crate::profile_store;
use crate::settings_link::{
    ChildKind, ChildMsg, InstallProgress, ParentMsg, child_command_line, decode_line,
};
use crate::settings_ui::{self, TickContext};
use crate::tray_ui::Tray;
use crate::update_ui::{Failure, UpdateEvent, handle_update_event, start_install};
use crate::updater::UpdateInfo;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
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

/// Writes the queued lines to the child's stdin on a thread of its own, so a
/// large message or a child that never reads cannot stall the caller. The
/// thread ends when the sender is dropped, or when a write fails (the child
/// is gone, which also unblocks a write stuck on a full pipe). `None` (logged)
/// if the thread could not be created; every later send is then dropped.
fn spawn_writer(mut stdin: Box<dyn Write + Send>) -> Option<Sender<Vec<u8>>> {
    let (tx, rx) = channel::<Vec<u8>>();
    let spawned = std::thread::Builder::new()
        .name("settings-link-out".into())
        .spawn(move || {
            for line in rx {
                if stdin.write_all(&line).and_then(|()| stdin.flush()).is_err() {
                    break;
                }
            }
        });
    if let Err(e) = spawned {
        log!(
            "settings link: couldn't start the writer thread ({e}); messages to the child are dropped"
        );
        return None;
    }
    Some(tx)
}

/// A running child: the Settings window or the first-run zone editor.
pub struct SettingsProcess<C: ProcessHandle = Child> {
    child: C,
    /// Lines for the writer thread (see [`spawn_writer`]); `None` once the
    /// pipe failed or was closed.
    stdin: Option<Sender<Vec<u8>>>,
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
            stdin: stdin.and_then(spawn_writer),
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

    /// Queues `msg` for the child; never blocks (the writer thread does the
    /// pipe write, which stalls while a child does not read). A failure (the
    /// child is gone) is dropped, `poll` reports the exit.
    pub fn send(&mut self, msg: &ParentMsg) {
        let Some(stdin) = self.stdin.as_ref() else {
            return;
        };
        let line = crate::settings_link::encode_line(msg);
        if stdin.send(line.into_bytes()).is_err() {
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

/// Handles what the Settings child sent since the last pass, and its exit.
///
/// The child owns the window, the tray process owns everything else: it
/// re-applies the Zone to running games (the per-window registry lives here),
/// forgets its cached config when the child wrote the file, and runs update
/// events for the tray.
///
/// Returns `true` when the child has exited (and `settings` is now `None`), so
/// the caller can apply what Settings may have changed.
pub(crate) fn service_settings(
    tray: &Tray,
    config_dir: &Path,
    settings: &mut Option<SettingsProcess>,
    offered_update: &mut Option<UpdateInfo>,
) -> bool {
    let Some(process) = settings.as_mut() else {
        return false;
    };
    let polled = process.poll();
    for msg in polled.messages {
        let reply = react_to_child_message(
            msg,
            &mut TrayEffects {
                tray,
                config_dir,
                context: &mut process.context,
                offered_update,
            },
        );
        if let Some(reply) = reply {
            process.send(&reply);
        }
    }
    if polled.exited {
        *settings = None;
        if polled.exit_code != Some(0) {
            log!("the Settings process ended with {:?}", polled.exit_code);
        }
    }
    polled.exited
}

/// What the tray process does on a Settings child's behalf; the real one is
/// [`TrayEffects`], the tests use a fake.
trait ChildEffects {
    /// The window now ticking, as the child reports it.
    fn set_context(&mut self, context: TickContext);
    /// The child wrote the config file: drop the cached copy.
    fn forget_config(&mut self);
    /// Live re-applies the saved Zone to the running tracked games and returns
    /// one status line per game that could not be repositioned.
    fn reapply_all(&mut self) -> Vec<String>;
    fn update_event(&mut self, event: UpdateEvent);
    /// Installs `info` on a worker thread, as the tray's own item would; the
    /// outcome reaches the child as [`ParentMsg::Install`] (see
    /// [`route_install_progress`]).
    fn start_install(&mut self, request: u64, info: UpdateInfo);
    fn notify(&mut self, text: &str);
}

/// Applies one message of the Settings child through `effects`; `Some` is the
/// answer to send back.
fn react_to_child_message(msg: ChildMsg, effects: &mut impl ChildEffects) -> Option<ParentMsg> {
    match msg {
        ChildMsg::Context(context) => effects.set_context(context),
        ChildMsg::ConfigChanged => effects.forget_config(),
        ChildMsg::ReapplyAll => {
            // The Zone was just saved by the child, behind this process's cache.
            effects.forget_config();
            return Some(ParentMsg::ReapplyDone(effects.reapply_all()));
        }
        ChildMsg::UpToDate => effects.update_event(UpdateEvent::UpToDate),
        ChildMsg::Install(request) => {
            let id = request.id;
            match request.into_info() {
                Some(info) => effects.start_install(id, info),
                None => {
                    return Some(ParentMsg::Install {
                        request: Some(id),
                        progress: InstallProgress::NotStarted,
                    });
                }
            }
        }
        ChildMsg::Notify(text) => effects.notify(&text),
    }
    None
}

/// The tray process's real [`ChildEffects`].
struct TrayEffects<'a> {
    tray: &'a Tray,
    config_dir: &'a Path,
    context: &'a mut TickContext,
    offered_update: &'a mut Option<UpdateInfo>,
}

impl ChildEffects for TrayEffects<'_> {
    fn set_context(&mut self, context: TickContext) {
        *self.context = context;
    }

    fn forget_config(&mut self) {
        profile_store::forget_cached(self.config_dir);
    }

    fn reapply_all(&mut self) -> Vec<String> {
        crate::watchdog::step("settings process: live re-apply");
        match profile_store::load(self.config_dir) {
            Ok(config) => settings_ui::live_reapply_all(self.config_dir, &config.tracked_games),
            Err(e) => {
                log!("live re-apply: failed to load the config: {e}");
                vec![format!("couldn't read the settings: {e}")]
            }
        }
    }

    fn update_event(&mut self, event: UpdateEvent) {
        handle_update_event(event, self.tray, self.config_dir, self.offered_update);
    }

    fn start_install(&mut self, request: u64, info: UpdateInfo) {
        // The install belongs to this process: closing Settings never stops it.
        start_install(info, Failure::Forward(request));
    }

    fn notify(&mut self, text: &str) {
        self.tray.notify(text);
    }
}

/// Where install progress goes, see [`route_install_progress`].
#[derive(Debug, PartialEq, Eq)]
enum InstallRoute {
    ToSettings(InstallProgress),
    /// Tell the user in a box.
    Box(String),
    Drop,
}

/// Decides where one piece of install progress goes. An open Settings window
/// shows it in its status line. Without one, only a failure matters: it is
/// shown in a box, so a failed install is never only in the log.
fn route_install_progress(settings_open: bool, progress: InstallProgress) -> InstallRoute {
    match progress {
        _ if settings_open => InstallRoute::ToSettings(progress),
        InstallProgress::Failed(text) => InstallRoute::Box(text),
        _ => InstallRoute::Drop,
    }
}

/// Hands the install progress `drain_events` collected to the Settings child, or
/// to a box when it is closed.
pub(crate) fn forward_install_progress(
    settings: &mut Option<SettingsProcess>,
    progress: Vec<(Option<u64>, InstallProgress)>,
) {
    for (request, progress) in progress {
        match route_install_progress(settings.is_some(), progress) {
            InstallRoute::ToSettings(progress) => {
                if let Some(process) = settings.as_mut() {
                    process.send(&ParentMsg::Install { request, progress });
                }
            }
            // A box blocks, and this is the event loop's thread.
            InstallRoute::Box(text) => {
                std::thread::spawn(move || crate::message_box::show_warning_box(&text));
            }
            InstallRoute::Drop => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::sync::Arc;

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
    struct Sent(Arc<(std::sync::Mutex<Vec<u8>>, std::sync::Condvar)>);

    impl Sent {
        /// Blocks (up to a generous bound, only hit on failure) until what was
        /// sent contains `needle`; returns everything sent so far.
        fn wait_for(&self, needle: &str) -> String {
            let (data, written) = &*self.0;
            let (data, _) = written
                .wait_timeout_while(data.lock().unwrap(), Duration::from_secs(30), |d| {
                    !String::from_utf8_lossy(d).contains(needle)
                })
                .unwrap();
            String::from_utf8_lossy(&data).into_owned()
        }
    }

    impl Write for Sent {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let (data, written) = &*self.0;
            data.lock().unwrap().extend_from_slice(buf);
            written.notify_all();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

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
        // The writer thread delivers it a moment later.
        let sent = rig.sent.wait_for("Close");
        assert!(
            sent.contains("Close"),
            "the child is asked to close first: {sent:?}"
        );
    }

    /// A stdin whose pipe is full: the first write announces itself on
    /// `entered`, then blocks until `release` yields.
    struct StuckStdin {
        entered: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    }

    impl Write for StuckStdin {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let _ = self.entered.send(());
            let _ = self.release.recv();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn sending_never_blocks_on_a_child_that_does_not_read() {
        let (entered_tx, entered) = channel();
        let (release_tx, release) = channel();
        let (_to_tray, events) = channel();
        let mut process = SettingsProcess::from_parts(
            FakeChild {
                exit_code: Rc::new(Cell::new(None)),
                killed: Rc::new(Cell::new(false)),
                check_fails: Rc::new(Cell::new(false)),
            },
            Some(Box::new(StuckStdin {
                entered: entered_tx,
                release,
            })),
            events,
            TickContext::Settings,
            None,
        );
        process.close_grace = Duration::from_millis(10);

        process.send(&ParentMsg::CursorEnabled(true));
        // The writer thread is now provably stuck inside the pipe write.
        entered.recv().unwrap();
        // Were `send` to write itself, it would block here forever.
        for _ in 0..1000 {
            process.send(&ParentMsg::CursorEnabled(true));
        }
        drop(release_tx); // unblocks the stuck write (recv errs) for teardown
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

    #[derive(Default)]
    struct FakeEffects {
        calls: Vec<String>,
        answer: Vec<String>,
    }

    impl ChildEffects for FakeEffects {
        fn set_context(&mut self, context: TickContext) {
            self.calls.push(format!("context {context:?}"));
        }
        fn forget_config(&mut self) {
            self.calls.push("forget".into());
        }
        fn reapply_all(&mut self) -> Vec<String> {
            self.calls.push("reapply".into());
            self.answer.clone()
        }
        fn update_event(&mut self, event: UpdateEvent) {
            self.calls
                .push(format!("update {}", matches!(event, UpdateEvent::UpToDate)));
        }
        fn start_install(&mut self, request: u64, info: UpdateInfo) {
            self.calls
                .push(format!("install {} #{request}", info.version));
        }
        fn notify(&mut self, text: &str) {
            self.calls.push(format!("notify {text}"));
        }
    }

    fn react(msg: ChildMsg) -> (Option<ParentMsg>, Vec<String>) {
        let mut effects = FakeEffects {
            answer: vec!["Game is not responding".to_string()],
            ..FakeEffects::default()
        };
        let reply = react_to_child_message(msg, &mut effects);
        (reply, effects.calls)
    }

    #[test]
    fn a_config_change_drops_the_cache_and_needs_no_answer() {
        assert_eq!(
            react(ChildMsg::ConfigChanged),
            (None, vec!["forget".to_string()])
        );
    }

    #[test]
    fn a_reapply_request_reloads_the_config_first_and_answers_with_the_status_lines() {
        let (reply, calls) = react(ChildMsg::ReapplyAll);

        assert_eq!(calls, vec!["forget".to_string(), "reapply".to_string()]);
        assert_eq!(
            reply,
            Some(ParentMsg::ReapplyDone(vec![
                "Game is not responding".to_string()
            ]))
        );
    }

    #[test]
    fn the_childs_context_update_and_notice_reach_the_tray_side() {
        assert_eq!(
            react(ChildMsg::Context(TickContext::SettingsCapturing)),
            (None, vec!["context SettingsCapturing".to_string()])
        );
        assert_eq!(
            react(ChildMsg::UpToDate),
            (None, vec!["update true".to_string()])
        );
        assert_eq!(
            react(ChildMsg::Notify("Hi".into())),
            (None, vec!["notify Hi".to_string()])
        );
    }

    fn sample_update() -> UpdateInfo {
        UpdateInfo {
            version: crate::updater::Version::parse("1.2.3").unwrap(),
            download_url: "https://github.com/x/y/Setup.exe".to_string(),
            sha256: "ab".repeat(32),
            signature_url: "https://github.com/x/y/Setup.exe.minisig".to_string(),
        }
    }

    #[test]
    fn an_install_request_is_run_by_the_tray_process_not_the_child() {
        let request = crate::settings_link::InstallRequest::new(5, &sample_update());
        assert_eq!(
            react(ChildMsg::Install(request)),
            (None, vec!["install 1.2.3 #5".to_string()])
        );
    }

    #[test]
    fn an_install_request_with_an_unreadable_version_is_answered_as_not_started() {
        let mut request = crate::settings_link::InstallRequest::new(5, &sample_update());
        request.version = "garbage".to_string();
        assert_eq!(
            react(ChildMsg::Install(request)),
            (
                Some(ParentMsg::Install {
                    request: Some(5),
                    progress: InstallProgress::NotStarted
                }),
                Vec::new()
            )
        );
    }

    #[test]
    fn install_progress_goes_to_an_open_settings_window_and_a_failure_to_a_box_without_one() {
        let failed = InstallProgress::Failed("disk full".into());
        let busy = InstallProgress::Busy;
        assert_eq!(
            route_install_progress(true, failed.clone()),
            InstallRoute::ToSettings(failed.clone())
        );
        assert_eq!(
            route_install_progress(true, busy.clone()),
            InstallRoute::ToSettings(busy.clone())
        );
        assert_eq!(
            route_install_progress(false, failed),
            InstallRoute::Box("disk full".to_string())
        );
        assert_eq!(route_install_progress(false, busy), InstallRoute::Drop);
        assert_eq!(
            route_install_progress(false, InstallProgress::Launched),
            InstallRoute::Drop
        );
    }
}
