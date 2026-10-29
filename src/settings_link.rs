//! The link between the tray process and its short-lived Settings / zone
//! editor child (`gameframe.exe --settings`, `--zone-editor`).
//!
//! Why a child (#186): eframe/wgpu loads the GPU user-mode driver, which stays
//! resident (~113 MB private bytes) after the window is dropped. Running the
//! windows in a process that exits when they close means the tray process never
//! loads it.
//!
//! The tray process keeps hotkeys, the tray, the hooks and the per-window
//! registry; the child only draws and edits. They talk over the child's
//! stdin/stdout, one JSON message per line ([`ParentMsg`] down, [`ChildMsg`]
//! up). Results otherwise travel through the config file (Save, a saved or
//! cleared Zone); the child exiting is the "closed" signal. The child never
//! takes the single-instance mutex, so it runs next to the tray process.
//!
//! This module is the wire format plus the child's end of the link; the tray
//! process's end is [`crate::settings_host`].

use crate::cursor_style::Bitmap;
use crate::hotkeys::BindingReport;
use crate::settings_ui::TickContext;
use crate::update_ui::UpdateEvent;
use crate::updater::{InstallStartError, UpdateInfo, Version};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Runs the Settings window in this process, then exits.
pub const SETTINGS_FLAG: &str = "--settings";
/// Runs the first-run zone editor in this process, then exits.
pub const ZONE_EDITOR_FLAG: &str = "--zone-editor";
/// The tray process talks to this child over its stdin/stdout. Without it the
/// child runs standalone (no live re-apply, no hotkey coordination); only a
/// debug build allows that, see [`ChildArgs::allowed_in`].
pub const LINKED_FLAG: &str = "--linked";

/// Exit code of `--zone-editor` when the user saved a Zone.
pub const EXIT_ZONE_SAVED: i32 = 10;

/// How long the child waits for the tray process's [`ParentMsg::Init`].
const INIT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the child waits for the tray process to live re-apply the Zone
/// (it contacts every running tracked game, which can take a few seconds).
const REAPPLY_TIMEOUT: Duration = Duration::from_secs(30);
/// How long an install request may stay unanswered before later progress of a
/// tray install is shown again (matches the installer's own fallback).
const PENDING_INSTALL_TIMEOUT: Duration = Duration::from_secs(120);

/// Which window a child process shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildKind {
    Settings,
    ZoneEditor,
}

/// A child process's command line, see [`parse_child_args`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildArgs {
    pub kind: ChildKind,
    pub linked: bool,
}

impl ChildArgs {
    /// Whether this build may run the window. Only the tray process' own
    /// (`--linked`) children are meant to exist: a standalone one takes no
    /// single-instance mutex and writes the config behind the tray's back, so it
    /// is for debug builds (manual testing) only.
    pub fn allowed_in(&self, debug_build: bool) -> bool {
        self.linked || debug_build
    }
}

/// Recognizes a child process's arguments (program name first). `None` for a
/// normal launch.
pub fn parse_child_args<I, S>(args: I) -> Option<ChildArgs>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args: Vec<S> = args.into_iter().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a.as_ref() == flag);
    let kind = if has(SETTINGS_FLAG) {
        ChildKind::Settings
    } else if has(ZONE_EDITOR_FLAG) {
        ChildKind::ZoneEditor
    } else {
        return None;
    };
    Some(ChildArgs {
        kind,
        linked: has(LINKED_FLAG),
    })
}

/// The command-line arguments the tray process starts a linked child with.
pub fn child_command_line(kind: ChildKind) -> [&'static str; 2] {
    let flag = match kind {
        ChildKind::Settings => SETTINGS_FLAG,
        ChildKind::ZoneEditor => ZONE_EDITOR_FLAG,
    };
    [flag, LINKED_FLAG]
}

/// What a child reports to the tray process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ChildMsg {
    /// Which window is ticking now; decides which hotkeys the tray process
    /// still runs (see `app::hotkey_allowed`).
    Context(TickContext),
    /// The Zone was saved: live re-apply it to the running tracked games and
    /// answer with [`ParentMsg::ReapplyDone`].
    ReapplyAll,
    /// The child changed the config file; drop the cached copy.
    ConfigChanged,
    /// A manual update check found no newer release (the tray hides its
    /// "Update to" item).
    UpToDate,
    /// Install this update. The tray process runs the install, so closing
    /// Settings can't interrupt it; progress comes back as
    /// [`ParentMsg::Install`].
    Install(InstallRequest),
    /// Show this text as a tray notification.
    Notify(String),
}

/// What the tray process tells a child.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ParentMsg {
    /// First message: what the child cannot find out on its own.
    Init {
        hotkey_reports: Vec<BindingReport>,
        game_cursor: Option<Bitmap>,
    },
    /// The game-cursor hotkey toggled `cursor.enabled` meanwhile.
    CursorEnabled(bool),
    /// The answer to [`ChildMsg::ReapplyAll`]: one status line per game that
    /// could not be repositioned.
    ReapplyDone(Vec<String>),
    /// How an install is going. `request` is the id of the
    /// [`ChildMsg::Install`] it answers; `None` for an install the tray
    /// started itself.
    Install {
        request: Option<u64>,
        progress: InstallProgress,
    },
    /// Close the window (discarding unsaved edits) and exit.
    Close,
}

/// An [`UpdateInfo`] on the wire. The tray process checks the download host,
/// the digest and the signature itself, so it trusts nothing in here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallRequest {
    /// Tags the answering [`ParentMsg::Install`] messages.
    pub(crate) id: u64,
    pub(crate) version: String,
    download_url: String,
    sha256: String,
    signature_url: String,
}

impl InstallRequest {
    pub(crate) fn new(id: u64, info: &UpdateInfo) -> Self {
        Self {
            id,
            version: info.version.to_string(),
            download_url: info.download_url.clone(),
            sha256: info.sha256.clone(),
            signature_url: info.signature_url.clone(),
        }
    }

    /// The update to install; `None` for an unparsable version.
    pub(crate) fn into_info(self) -> Option<UpdateInfo> {
        Some(UpdateInfo {
            version: Version::parse(&self.version).ok()?,
            download_url: self.download_url,
            sha256: self.sha256,
            signature_url: self.signature_url,
        })
    }
}

/// What the tray process tells Settings about the install it was asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstallProgress {
    /// The installer was started; it asks GameFrame to quit.
    Launched,
    /// The install failed; the text is for the user.
    Failed(String),
    /// Not started: another install is running.
    Busy,
    /// Not started: the worker thread could not be created, or the request
    /// was unusable.
    NotStarted,
    /// Not started: this instance is elevated and never installs.
    Elevated,
}

impl InstallProgress {
    /// What `event` means for an open Settings window, with the id of the
    /// request it answers (`None`: a tray install); `None` if nothing.
    pub(crate) fn from_event(event: &UpdateEvent) -> Option<(Option<u64>, Self)> {
        match event {
            UpdateEvent::InstallerLaunched(_, request) => Some((*request, Self::Launched)),
            UpdateEvent::InstallError(text, id) => Some((Some(*id), Self::Failed(text.clone()))),
            UpdateEvent::InstallNotStarted(why, id) => Some((
                Some(*id),
                match why {
                    InstallStartError::Busy => Self::Busy,
                    InstallStartError::ThreadFailed => Self::NotStarted,
                    InstallStartError::Elevated => Self::Elevated,
                },
            )),
            _ => None,
        }
    }
}

/// One wire line: the message as compact JSON plus the newline. JSON escapes
/// newlines inside strings, so a message is always exactly one line.
pub fn encode_line<T: Serialize>(msg: &T) -> String {
    let mut line = serde_json::to_string(msg).unwrap_or_default();
    line.push('\n');
    line
}

/// Decodes one wire line; `None` for anything that is not a message of this
/// build (ignored by both ends).
pub fn decode_line<T: DeserializeOwned>(line: &str) -> Option<T> {
    serde_json::from_str(line.trim()).ok()
}

/// What the child has received but not consumed yet.
struct Inbox {
    rx: Receiver<ParentMsg>,
    /// The latest unconsumed [`ParentMsg::CursorEnabled`].
    cursor_enabled: Option<bool>,
    /// The [`ParentMsg::Init`] data, once it has arrived.
    init: Option<(Vec<BindingReport>, Option<Bitmap>)>,
    /// Install progress not taken yet, oldest first.
    install: Vec<InstallProgress>,
    /// The id of the install request this window made and has no answer to yet,
    /// and when it was made. Given up on after [`PENDING_INSTALL_TIMEOUT`].
    pending_install: Option<(u64, Instant)>,
    /// The last context reported, so only changes are sent.
    last_context: Option<TickContext>,
}

impl Inbox {
    /// Files a message that nobody is waiting for specifically.
    fn file(&mut self, msg: ParentMsg) {
        match msg {
            ParentMsg::Init {
                hotkey_reports,
                game_cursor,
            } => self.init = Some((hotkey_reports, game_cursor)),
            ParentMsg::CursorEnabled(enabled) => self.cursor_enabled = Some(enabled),
            ParentMsg::Install { request, progress } => {
                self.file_install(request, progress, Instant::now());
            }
            // A stray answer (its request timed out) or `Close`, which the
            // reader thread already turned into `on_close`.
            ParentMsg::ReapplyDone(_) | ParentMsg::Close => {}
        }
    }

    /// Keeps `progress` only if it is for this window: the answer to the
    /// pending request, or, with none pending, progress of a tray install
    /// (`request` is `None`). Anything else (a tray install that finished
    /// meanwhile, a stale answer) must not become the pending request's result.
    /// A request unanswered for [`PENDING_INSTALL_TIMEOUT`] at `now` stops
    /// hiding tray progress (the link may have broken mid-request), but its id is
    /// kept: a slow install's own late answer is still applied.
    fn file_install(&mut self, request: Option<u64>, progress: InstallProgress, now: Instant) {
        let expired = self.pending_install.is_none_or(|(_, asked)| {
            now.saturating_duration_since(asked) >= PENDING_INSTALL_TIMEOUT
        });
        match (self.pending_install, request) {
            (Some((pending, _)), Some(id)) if pending == id => {
                self.pending_install = None;
                self.install.push(progress);
            }
            (_, None) if expired => self.install.push(progress),
            _ => {}
        }
    }
}

/// The child's end of the link.
pub struct ChildLink {
    out: Mutex<Box<dyn Write + Send>>,
    inbox: Mutex<Inbox>,
    /// The id of the next [`InstallRequest`].
    next_request: AtomicU64,
}

impl ChildLink {
    /// Starts reading `reader` on a thread of its own. `on_close` runs when the
    /// tray process sends [`ParentMsg::Close`] or the pipe ends (the tray
    /// process exited): the child should then close its window.
    pub fn new(
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
        on_close: impl Fn() + Send + 'static,
    ) -> Self {
        let (tx, rx) = channel();
        let spawned = std::thread::Builder::new()
            .name("parent-link".into())
            .spawn(move || {
                for line in BufReader::new(reader).lines() {
                    let Ok(line) = line else { break };
                    match decode_line::<ParentMsg>(&line) {
                        Some(ParentMsg::Close) => on_close(),
                        Some(msg) => {
                            if tx.send(msg).is_err() {
                                break;
                            }
                        }
                        None => {}
                    }
                }
                on_close();
            });
        if let Err(e) = spawned {
            log!("couldn't start the link reader: {e}");
        }
        Self {
            out: Mutex::new(Box::new(writer)),
            inbox: Mutex::new(Inbox {
                rx,
                cursor_enabled: None,
                init: None,
                install: Vec::new(),
                pending_install: None,
                last_context: None,
            }),
            next_request: AtomicU64::new(1),
        }
    }

    fn lock_inbox(&self) -> std::sync::MutexGuard<'_, Inbox> {
        self.inbox.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Tells the tray process at once that `context` is now ticking (instead
    /// of with the next [`Self::tick`], up to a frame later), for a change that
    /// must take effect before the next hotkey press: a started capture, the
    /// zone editor opening.
    pub fn report_context(&self, context: TickContext) {
        let changed = {
            let mut inbox = self.lock_inbox();
            let changed = inbox.last_context != Some(context);
            inbox.last_context = Some(context);
            changed
        };
        if changed {
            self.send(&ChildMsg::Context(context));
        }
    }

    /// Sends `msg`; a failure (the tray process is gone) is only logged.
    pub fn send(&self, msg: &ChildMsg) {
        let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = out
            .write_all(encode_line(msg).as_bytes())
            .and_then(|()| out.flush())
        {
            log!("link to the tray process failed: {e}");
        }
    }

    /// Waits for the tray process's [`ParentMsg::Init`]; `None` if it does not
    /// come (standalone defaults apply then).
    pub fn wait_init(&self) -> Option<(Vec<BindingReport>, Option<Bitmap>)> {
        let deadline = Instant::now() + INIT_TIMEOUT;
        let mut inbox = self.lock_inbox();
        while inbox.init.is_none() {
            let left = deadline.saturating_duration_since(Instant::now());
            match inbox.rx.recv_timeout(left) {
                Ok(msg) => inbox.file(msg),
                Err(_) => break,
            }
        }
        inbox.init.take()
    }

    /// What the window's frame loop calls each frame: tells the tray process
    /// when `context` changed and returns the new `cursor.enabled` if its
    /// hotkey toggled it since the last call.
    pub fn tick(&self, context: TickContext) -> Option<bool> {
        self.report_context(context);
        let mut inbox = self.lock_inbox();
        while let Ok(msg) = inbox.rx.try_recv() {
            inbox.file(msg);
        }
        inbox.cursor_enabled.take()
    }

    /// Asks the tray process to install `info`. Progress comes back through
    /// [`Self::take_install_progress`]. Progress of an earlier request that was
    /// not taken is dropped.
    pub fn request_install(&self, info: &UpdateInfo) {
        let mut inbox = self.lock_inbox();
        while let Ok(msg) = inbox.rx.try_recv() {
            inbox.file(msg);
        }
        inbox.install.clear();
        let id = self.next_request.fetch_add(1, Ordering::Relaxed);
        inbox.pending_install = Some((id, Instant::now()));
        drop(inbox);
        self.send(&ChildMsg::Install(InstallRequest::new(id, info)));
    }

    /// The install progress the tray process reported since the last call.
    pub fn take_install_progress(&self) -> Vec<InstallProgress> {
        let mut inbox = self.lock_inbox();
        while let Ok(msg) = inbox.rx.try_recv() {
            inbox.file(msg);
        }
        std::mem::take(&mut inbox.install)
    }

    /// Asks the tray process to live re-apply the saved Zone and waits for its
    /// status lines. Empty if it does not answer in time.
    pub fn request_reapply(&self) -> Vec<String> {
        self.send(&ChildMsg::ReapplyAll);
        let deadline = Instant::now() + REAPPLY_TIMEOUT;
        let mut inbox = self.lock_inbox();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match inbox.rx.recv_timeout(left) {
                Ok(ParentMsg::ReapplyDone(messages)) => return messages,
                Ok(msg) => inbox.file(msg),
                Err(RecvTimeoutError::Timeout) => {
                    log!("the tray process did not answer the live re-apply request");
                    return Vec::new();
                }
                Err(RecvTimeoutError::Disconnected) => return Vec::new(),
            }
        }
    }
}

static LINK: OnceLock<ChildLink> = OnceLock::new();

/// Connects this child process to the tray process over its own stdin/stdout.
pub fn connect_stdio() -> &'static ChildLink {
    LINK.get_or_init(|| {
        ChildLink::new(
            std::io::stdin(),
            std::io::stdout(),
            crate::instance_channel::request_close,
        )
    })
}

/// The link, in a linked child process; `None` in the tray process and in a
/// standalone child.
pub fn link() -> Option<&'static ChildLink> {
    LINK.get()
}

/// Reports `context` to the tray process immediately (no-op without a link),
/// see [`ChildLink::report_context`].
pub fn report_context(context: TickContext) {
    if let Some(link) = link() {
        link.report_context(context);
    }
}

/// Tells the tray process the config file changed (no-op without a link).
pub fn config_changed() {
    if let Some(link) = link() {
        link.send(&ChildMsg::ConfigChanged);
    }
}

/// Tells the tray process a manual update check found no newer release (no-op
/// without a link).
pub(crate) fn report_up_to_date() {
    if let Some(link) = link() {
        link.send(&ChildMsg::UpToDate);
    }
}

/// Asks the tray process to install `info`; `false` without a link (a
/// standalone child has no tray process to run it).
pub(crate) fn request_install(info: &UpdateInfo) -> bool {
    let Some(link) = link() else {
        return false;
    };
    link.request_install(info);
    true
}

/// The install progress reported since the last call (empty without a link).
pub(crate) fn take_install_progress() -> Vec<InstallProgress> {
    link()
        .map(ChildLink::take_install_progress)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkeys::{BindingOutcome, HotkeyAction};
    use std::io::pipe;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    /// A link whose far end the test drives: `(link, to_child, from_child, closed)`.
    fn linked_pair() -> (
        ChildLink,
        std::io::PipeWriter,
        BufReader<std::io::PipeReader>,
        Arc<AtomicBool>,
    ) {
        let (child_in, to_child) = pipe().unwrap();
        let (from_child, child_out) = pipe().unwrap();
        let closed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&closed);
        let link = ChildLink::new(child_in, child_out, move || {
            flag.store(true, Ordering::SeqCst)
        });
        (link, to_child, BufReader::new(from_child), closed)
    }

    fn read_msg(from_child: &mut BufReader<std::io::PipeReader>) -> ChildMsg {
        let mut line = String::new();
        from_child.read_line(&mut line).unwrap();
        decode_line(&line).expect("a child message")
    }

    fn write_msg(to_child: &mut std::io::PipeWriter, msg: &ParentMsg) {
        to_child.write_all(encode_line(msg).as_bytes()).unwrap();
    }

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn child_arguments_are_recognized_after_the_program_name_only() {
        assert_eq!(
            parse_child_args(["gameframe.exe", "--settings", "--linked"]),
            Some(ChildArgs {
                kind: ChildKind::Settings,
                linked: true
            })
        );
        assert_eq!(
            parse_child_args(["gameframe.exe", "--zone-editor"]),
            Some(ChildArgs {
                kind: ChildKind::ZoneEditor,
                linked: false
            })
        );
        assert_eq!(parse_child_args(["gameframe.exe"]), None);
        assert_eq!(parse_child_args(["gameframe.exe", "--quit"]), None);
        assert_eq!(parse_child_args(["--settings"]), None);
    }

    #[test]
    fn the_command_line_the_tray_process_uses_parses_back_to_the_same_child() {
        for kind in [ChildKind::Settings, ChildKind::ZoneEditor] {
            let mut args = vec!["gameframe.exe".to_string()];
            args.extend(child_command_line(kind).map(String::from));
            assert_eq!(
                parse_child_args(args),
                Some(ChildArgs { kind, linked: true })
            );
        }
    }

    #[test]
    fn messages_round_trip_as_single_lines() {
        let up = ChildMsg::Notify("two\nlines".to_string());
        let line = encode_line(&up);
        assert_eq!(line.matches('\n').count(), 1, "one line per message");
        assert_eq!(decode_line::<ChildMsg>(&line), Some(up));

        let down = ParentMsg::Init {
            hotkey_reports: vec![BindingReport {
                action: HotkeyAction::ToggleBorderless,
                binding: "Ctrl+F9".to_string(),
                outcome: BindingOutcome::Failed("in use".to_string()),
            }],
            game_cursor: Some(Bitmap::blank(2, 2, 1, 1)),
        };
        assert_eq!(decode_line::<ParentMsg>(&encode_line(&down)), Some(down));
    }

    #[test]
    fn a_line_that_is_not_a_message_is_ignored() {
        assert_eq!(decode_line::<ChildMsg>("not json"), None);
        assert_eq!(decode_line::<ChildMsg>("{\"FromTheFuture\":1}"), None);
        assert_eq!(decode_line::<ChildMsg>(""), None);
    }

    fn sample_update() -> UpdateInfo {
        UpdateInfo {
            version: Version::parse("1.2.3").unwrap(),
            download_url: "https://github.com/ranarn/gameframe/releases/download/v1.2.3/Setup.exe"
                .to_string(),
            sha256: "ab".repeat(32),
            signature_url:
                "https://github.com/ranarn/gameframe/releases/download/v1.2.3/Setup.exe.minisig"
                    .to_string(),
        }
    }

    #[test]
    fn an_install_request_survives_the_wire_and_a_bad_version_is_refused() {
        let info = sample_update();
        let line = encode_line(&ChildMsg::Install(InstallRequest::new(4, &info)));
        let Some(ChildMsg::Install(request)) = decode_line::<ChildMsg>(&line) else {
            panic!("an install request");
        };
        assert_eq!(request.id, 4);
        assert_eq!(request.into_info(), Some(info));

        let mut bad = InstallRequest::new(1, &sample_update());
        bad.version = "garbage".to_string();
        assert_eq!(bad.into_info(), None);
    }

    #[test]
    fn only_install_events_become_progress_for_the_open_settings_window() {
        let version = Version::parse("1.2.3").unwrap();
        let progress = |event| InstallProgress::from_event(&event);
        assert_eq!(
            progress(UpdateEvent::InstallerLaunched(version, None)),
            Some((None, InstallProgress::Launched))
        );
        assert_eq!(
            progress(UpdateEvent::InstallerLaunched(version, Some(3))),
            Some((Some(3), InstallProgress::Launched))
        );
        assert_eq!(
            progress(UpdateEvent::InstallError("disk full".into(), 3)),
            Some((Some(3), InstallProgress::Failed("disk full".into())))
        );
        assert_eq!(
            progress(UpdateEvent::InstallNotStarted(InstallStartError::Busy, 3)),
            Some((Some(3), InstallProgress::Busy))
        );
        assert_eq!(
            progress(UpdateEvent::InstallNotStarted(
                InstallStartError::ThreadFailed,
                3
            )),
            Some((Some(3), InstallProgress::NotStarted))
        );
        assert_eq!(
            progress(UpdateEvent::InstallNotStarted(
                InstallStartError::Elevated,
                3
            )),
            Some((Some(3), InstallProgress::Elevated))
        );
        // Progress that needs no window of its own.
        assert_eq!(progress(UpdateEvent::InstallStarted(version)), None);
        assert_eq!(progress(UpdateEvent::InstallFailed), None);
        assert_eq!(progress(UpdateEvent::UpToDate), None);
    }

    #[test]
    fn an_install_request_goes_to_the_tray_process_and_its_progress_comes_back() {
        let (link, mut to_child, mut from_child, _closed) = linked_pair();
        link.request_install(&sample_update());
        let ChildMsg::Install(request) = read_msg(&mut from_child) else {
            panic!("an install request");
        };
        assert_eq!(request, InstallRequest::new(request.id, &sample_update()));

        write_msg(
            &mut to_child,
            &answer(Some(request.id), InstallProgress::Busy),
        );
        let mut seen = Vec::new();
        wait_until(|| {
            seen.extend(link.take_install_progress());
            !seen.is_empty()
        });
        assert_eq!(seen, vec![InstallProgress::Busy]);
        assert!(link.take_install_progress().is_empty(), "taken once");
    }

    fn answer(request: Option<u64>, progress: InstallProgress) -> ParentMsg {
        ParentMsg::Install { request, progress }
    }

    #[test]
    fn progress_of_a_tray_install_queued_ahead_of_the_answer_is_not_the_requests_result() {
        let (link, mut to_child, mut from_child, _closed) = linked_pair();
        link.request_install(&sample_update());
        let ChildMsg::Install(request) = read_msg(&mut from_child) else {
            panic!("an install request");
        };

        // The tray's own install launched first; the request was refused as busy.
        write_msg(&mut to_child, &answer(None, InstallProgress::Launched));
        write_msg(
            &mut to_child,
            &answer(Some(request.id), InstallProgress::Busy),
        );
        let mut seen = Vec::new();
        wait_until(|| {
            seen.extend(link.take_install_progress());
            !seen.is_empty()
        });
        // Whatever else is still on the way is not for this request either.
        write_msg(
            &mut to_child,
            &answer(Some(request.id + 7), InstallProgress::Launched),
        );
        write_msg(
            &mut to_child,
            &answer(Some(request.id), InstallProgress::Launched),
        );
        write_msg(&mut to_child, &ParentMsg::CursorEnabled(true));
        wait_until(|| link.tick(TickContext::Settings) == Some(true));

        assert_eq!(seen, vec![InstallProgress::Busy]);
        assert!(link.take_install_progress().is_empty());
    }

    #[test]
    fn progress_of_a_tray_install_shows_while_no_request_is_pending() {
        let (link, mut to_child, _from_child, _closed) = linked_pair();
        write_msg(&mut to_child, &answer(None, InstallProgress::Launched));
        write_msg(&mut to_child, &answer(Some(9), InstallProgress::Busy));
        write_msg(&mut to_child, &ParentMsg::CursorEnabled(true));
        wait_until(|| link.tick(TickContext::Settings) == Some(true));

        assert_eq!(
            link.take_install_progress(),
            vec![InstallProgress::Launched]
        );
    }

    #[test]
    fn an_unanswered_install_request_stops_hiding_tray_progress_after_the_timeout() {
        let (link, _to_child, _from_child, _closed) = linked_pair();
        link.request_install(&sample_update());
        let mut inbox = link.lock_inbox();
        let asked = inbox.pending_install.unwrap().1;

        let soon = asked + Duration::from_secs(1);
        inbox.file_install(None, InstallProgress::Launched, soon);
        assert!(inbox.install.is_empty(), "still pending: not shown");

        inbox.file_install(
            None,
            InstallProgress::Launched,
            asked + PENDING_INSTALL_TIMEOUT,
        );
        assert_eq!(inbox.install, vec![InstallProgress::Launched]);
    }

    #[test]
    fn the_answer_to_a_slow_install_is_applied_after_the_timeout() {
        let (link, _to_child, _from_child, _closed) = linked_pair();
        link.request_install(&sample_update());
        let mut inbox = link.lock_inbox();
        let (id, asked) = inbox.pending_install.unwrap();

        inbox.file_install(
            Some(id),
            InstallProgress::Launched,
            asked + PENDING_INSTALL_TIMEOUT + Duration::from_secs(5),
        );
        assert_eq!(inbox.install, vec![InstallProgress::Launched]);
        assert!(inbox.pending_install.is_none());
    }

    #[test]
    fn a_new_install_request_drops_progress_left_over_from_the_last_one() {
        let (link, mut to_child, _from_child, _closed) = linked_pair();
        write_msg(&mut to_child, &answer(None, InstallProgress::Busy));
        // The stale line is read by the link before the new request clears it.
        wait_until(|| {
            let mut inbox = link.lock_inbox();
            while let Ok(msg) = inbox.rx.try_recv() {
                inbox.file(msg);
            }
            !inbox.install.is_empty()
        });

        link.request_install(&sample_update());

        assert!(link.take_install_progress().is_empty());
    }

    #[test]
    fn the_init_message_reaches_the_child() {
        let (link, mut to_child, _from_child, _closed) = linked_pair();
        write_msg(
            &mut to_child,
            &ParentMsg::Init {
                hotkey_reports: Vec::new(),
                game_cursor: None,
            },
        );
        assert_eq!(link.wait_init(), Some((Vec::new(), None)));
    }

    #[test]
    fn a_tick_reports_the_context_once_per_change() {
        let (link, _to_child, mut from_child, _closed) = linked_pair();
        link.tick(TickContext::Settings);
        link.tick(TickContext::Settings);
        link.tick(TickContext::ZoneEditor);
        assert_eq!(
            read_msg(&mut from_child),
            ChildMsg::Context(TickContext::Settings)
        );
        assert_eq!(
            read_msg(&mut from_child),
            ChildMsg::Context(TickContext::ZoneEditor)
        );
    }

    #[test]
    fn a_context_reported_at_once_is_not_repeated_by_the_next_tick() {
        let (link, _to_child, mut from_child, _closed) = linked_pair();
        link.report_context(TickContext::SettingsCapturing);
        link.tick(TickContext::SettingsCapturing);
        link.tick(TickContext::Settings);
        assert_eq!(
            read_msg(&mut from_child),
            ChildMsg::Context(TickContext::SettingsCapturing)
        );
        assert_eq!(
            read_msg(&mut from_child),
            ChildMsg::Context(TickContext::Settings)
        );
    }

    #[test]
    fn only_linked_children_run_in_a_release_build() {
        let linked = parse_child_args(["gameframe.exe", "--settings", "--linked"]).unwrap();
        let standalone = parse_child_args(["gameframe.exe", "--settings"]).unwrap();
        assert!(linked.allowed_in(false));
        assert!(!standalone.allowed_in(false));
        assert!(standalone.allowed_in(true));
    }

    #[test]
    fn a_cursor_toggle_from_the_tray_process_is_returned_by_one_tick_only() {
        let (link, mut to_child, _from_child, _closed) = linked_pair();
        write_msg(&mut to_child, &ParentMsg::CursorEnabled(true));
        wait_until(|| link.tick(TickContext::Settings) == Some(true));
        assert_eq!(link.tick(TickContext::Settings), None);
    }

    #[test]
    fn a_reapply_request_waits_for_the_answer_and_keeps_other_messages() {
        let (link, mut to_child, mut from_child, _closed) = linked_pair();
        let parent = std::thread::spawn(move || {
            assert_eq!(read_msg(&mut from_child), ChildMsg::ReapplyAll);
            write_msg(&mut to_child, &ParentMsg::CursorEnabled(true));
            write_msg(
                &mut to_child,
                &ParentMsg::ReapplyDone(vec!["Game is not responding".to_string()]),
            );
            to_child
        });
        assert_eq!(
            link.request_reapply(),
            vec!["Game is not responding".to_string()]
        );
        let _keep_open = parent.join().unwrap();
        assert_eq!(
            link.tick(TickContext::Settings),
            Some(true),
            "the toggle that arrived during the request is not lost"
        );
    }

    #[test]
    fn close_and_a_vanished_tray_process_both_close_the_child() {
        let (link, mut to_child, _from_child, closed) = linked_pair();
        write_msg(&mut to_child, &ParentMsg::Close);
        wait_until(|| closed.load(Ordering::SeqCst));
        drop(link);

        let (_link, to_child, _from_child, closed) = linked_pair();
        drop(to_child); // the tray process exited: EOF
        wait_until(|| closed.load(Ordering::SeqCst));
    }
}
