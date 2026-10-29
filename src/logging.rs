//! GameFrame's log file, `gameframe.log` in the config directory.
//!
//! The release build is a `windows_subsystem = "windows"` app with no
//! console, so `eprintln!` output goes nowhere; every diagnostic goes through
//! [`log!`] instead, which writes a timestamped line to the file (and to
//! stderr, for `cargo run`). The file is capped at [`MAX_LOG_BYTES`]: when a
//! write would pass the cap, the current file becomes `gameframe.old.log`
//! (replacing any older one) and a fresh file starts, so at most two files'
//! worth is ever kept.
//!
//! [`init`] also installs a `log` crate backend ([`LogBridge`]) so records
//! from dependencies (egui-wgpu, wgpu, ...) land in the same file:
//! `warn`/`error` from every crate, plus `info` from the GPU crates. Repeated
//! identical records (egui-wgpu warns on every dropped frame) are logged once
//! per [`REPEAT_WINDOW`], followed by a count of the suppressed ones. winit
//! 0.30 logs through `tracing`, which this bridge does not see. The GPU crates
//! log little at `info`, so the zone-editor overlay logs the adapter it got
//! itself.
//!
//! Before [`init`] runs (and in tests) lines go to stderr only.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Writes one timestamped line to the log file, `format!`-style.
macro_rules! log {
    ($($arg:tt)*) => {
        $crate::logging::write(format_args!($($arg)*))
    };
}

pub const LOG_FILE: &str = "gameframe.log";
pub const OLD_LOG_FILE: &str = "gameframe.old.log";
pub const MAX_LOG_BYTES: u64 = 1024 * 1024;

static LOG: Mutex<Option<LogFile>> = Mutex::new(None);

/// Opens (appending to) the log file in `dir`. Failing to open it only
/// costs the file output; logging to stderr keeps working. Idempotent: once a
/// file is open, later calls do nothing (an elevated relaunch initialises
/// early in `main` and again in `app::run`).
pub fn init(dir: &Path) {
    install_log_bridge();
    let mut slot = LOG.lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_some() {
        return;
    }
    match LogFile::open(dir, MAX_LOG_BYTES) {
        Ok(file) => *slot = Some(file),
        Err(e) => eprintln!("could not open the log file in {}: {e}", dir.display()),
    }
}

/// Marks this process as the single-instance owner, which lets it rotate the
/// log. Until called, a process only appends. Call after [`init`]: before it
/// there is no log file to mark, and the call does nothing.
pub fn claim_rotation() {
    if let Some(file) = LOG.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        file.owner = true;
    }
}

/// Crates whose `info` records are forwarded (see [`should_forward`]).
const INFO_TARGET_PREFIXES: [&str; 3] = ["egui_wgpu", "wgpu_core", "wgpu_hal"];

/// Whether a `log` crate record is written to the log file: `warn` and
/// `error` from every crate, `info` only from the GPU crates;
/// `debug`/`trace` never, so there is no per-frame noise. A target matches a prefix as a whole path segment
/// (`wgpu_hal::dx12` yes, `wgpu_hal_extra` no).
fn should_forward(level: log::Level, target: &str) -> bool {
    match level {
        log::Level::Error | log::Level::Warn => true,
        log::Level::Info => INFO_TARGET_PREFIXES.iter().any(|p| {
            target == *p
                || target
                    .strip_prefix(p)
                    .is_some_and(|rest| rest.starts_with("::"))
        }),
        log::Level::Debug | log::Level::Trace => false,
    }
}

/// How long an identical forwarded record is suppressed after being logged.
const REPEAT_WINDOW: Duration = Duration::from_secs(60);
/// Distinct records remembered at once; the memory is cleared when exceeded.
const MAX_TRACKED: usize = 64;

/// What to do with a forwarded record.
#[derive(Debug, PartialEq, Eq)]
enum Repeat {
    /// Log it, after `n` identical records were suppressed since the last one.
    Log(u32),
    Suppress,
}

/// Rate-limits identical records: first one passes, repeats within
/// [`REPEAT_WINDOW`] are counted, and the next one after it reports the count.
#[derive(Default)]
struct RepeatFilter {
    seen: HashMap<String, (Instant, u32)>,
    /// Counts of records dropped from `seen` while still pending, waiting to
    /// be written. Drained by [`LogBridge::log`] after every `check` and by
    /// [`RepeatFilter::take_pending`], so it stays small.
    evicted: Vec<(String, u32)>,
}

/// The line reporting `n` suppressed repeats of `key`.
fn repeated_line(key: &str, n: u32) -> String {
    format!("{key} (repeated {n} more times)")
}

impl RepeatFilter {
    /// Every pending count line: those of records evicted since the last call
    /// and those still tracked. Resets the tracked counts to zero.
    fn take_pending(&mut self) -> Vec<String> {
        let mut lines: Vec<String> = std::mem::take(&mut self.evicted)
            .into_iter()
            .map(|(key, n)| repeated_line(&key, n))
            .collect();
        for (key, (_, n)) in self.seen.iter_mut() {
            if *n > 0 {
                lines.push(repeated_line(key, *n));
                *n = 0;
            }
        }
        // Deterministic order regardless of the map's iteration order.
        lines.sort();
        lines
    }

    fn check(&mut self, key: &str, now: Instant) -> Repeat {
        if let Some((last, suppressed)) = self.seen.get_mut(key) {
            if now.duration_since(*last) < REPEAT_WINDOW {
                *suppressed += 1;
                return Repeat::Suppress;
            }
            let n = *suppressed;
            *last = now;
            *suppressed = 0;
            return Repeat::Log(n);
        }
        if self.seen.len() >= MAX_TRACKED {
            self.evicted.extend(
                self.seen
                    .drain()
                    .filter(|(_, (_, n))| *n > 0)
                    .map(|(k, (_, n))| (k, n)),
            );
        }
        self.seen.insert(key.to_string(), (now, 0));
        Repeat::Log(0)
    }
}

static REPEATS: Mutex<Option<RepeatFilter>> = Mutex::new(None);

/// `log::Log` implementation that writes through [`write`].
struct LogBridge;

impl log::Log for LogBridge {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        should_forward(metadata.level(), metadata.target())
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!("{} {}: {}", record.level(), record.target(), record.args());
        let (decision, evicted) = {
            let mut guard = REPEATS.lock().unwrap_or_else(|e| e.into_inner());
            let filter = guard.get_or_insert_with(RepeatFilter::default);
            let decision = filter.check(&line, Instant::now());
            (decision, std::mem::take(&mut filter.evicted))
        };
        for (key, n) in evicted {
            write(format_args!("{}", repeated_line(&key, n)));
        }
        match decision {
            Repeat::Suppress => {}
            Repeat::Log(0) => write(format_args!("{line}")),
            Repeat::Log(n) => write(format_args!("{}", repeated_line(&line, n))),
        }
    }

    /// Writes every pending "(repeated N more times)" line.
    fn flush(&self) {
        let lines = REPEATS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
            .map(RepeatFilter::take_pending)
            .unwrap_or_default();
        for line in lines {
            write(format_args!("{line}"));
        }
    }
}

/// Writes the pending repeat counts of the `log` crate bridge; call once on
/// graceful exit.
pub fn flush() {
    log::Log::flush(&LogBridge);
}

/// Installs [`LogBridge`] as the global `log` logger.
fn install_log_bridge() {
    // A second `init` (or another logger) makes this fail, which is fine.
    if log::set_logger(&LogBridge).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
}

/// The [`log!`] macro's backend.
pub fn write(args: std::fmt::Arguments) {
    let line = format!("{} [{}] {args}\n", timestamp(), thread_name());
    eprint!("{line}");
    if let Some(file) = LOG.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        file.append(&line);
    }
}

/// Tags this process's log lines (the Settings child writes the same file as
/// the tray process), so they can be told apart. First call wins.
pub fn set_process_tag(tag: &'static str) {
    let _ = PROCESS_TAG.set(tag);
}

static PROCESS_TAG: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();

fn thread_name() -> String {
    let thread = std::thread::current();
    let name = thread
        .name()
        .map(str::to_string)
        .unwrap_or_else(|| format!("{:?}", thread.id()));
    tagged(PROCESS_TAG.get().copied(), &name)
}

/// `name`, prefixed with the process tag when there is one.
fn tagged(tag: Option<&str>, name: &str) -> String {
    match tag {
        Some(tag) => format!("{tag} {name}"),
        None => name.to_string(),
    }
}

/// Local wall-clock time, `YYYY-MM-DD hh:mm:ss.mmm`.
fn timestamp() -> String {
    // Safety: GetLocalTime only fills the out-struct.
    let t = unsafe {
        let mut t = std::mem::zeroed();
        windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut t);
        t
    };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    )
}

/// The open log file plus what's needed to rotate it.
struct LogFile {
    dir: PathBuf,
    max_bytes: u64,
    file: File,
    size: u64,
    /// Only the process owning the single-instance mutex rotates; any other
    /// (a second launch, an elevated relaunch still waiting) only appends,
    /// because the owner holds the file open and the rename would fail.
    owner: bool,
    rotate_failure_noted: bool,
    /// When the last rotation attempt failed; see [`rotation_due`].
    last_failed_rotation: Option<Instant>,
}

/// After a failed rotation, the rename is retried at most this often.
const ROTATE_RETRY: Duration = Duration::from_secs(60);

/// Whether a rotation may be attempted now: always, unless the last attempt
/// failed less than [`ROTATE_RETRY`] ago.
fn rotation_due(last_failure: Option<Instant>, now: Instant) -> bool {
    last_failure.is_none_or(|t| now.duration_since(t) >= ROTATE_RETRY)
}

impl LogFile {
    fn open(dir: &Path, max_bytes: u64) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(LOG_FILE))?;
        let size = file.metadata()?.len();
        Ok(LogFile {
            dir: dir.to_path_buf(),
            max_bytes,
            file,
            size,
            owner: false,
            rotate_failure_noted: false,
            last_failed_rotation: None,
        })
    }

    /// Appends `line`, rotating first if this process owns the log and the line
    /// would pass the cap. A failed write is dropped: there is nowhere left to
    /// report it.
    fn append(&mut self, line: &str) {
        if self.owner
            && self.size > 0
            && self.size + line.len() as u64 > self.max_bytes
            && rotation_due(self.last_failed_rotation, Instant::now())
        {
            self.rotate();
        }
        if self.file.write_all(line.as_bytes()).is_ok() {
            self.size += line.len() as u64;
        }
    }

    /// Moves the current file aside and starts a new one. If the rename fails
    /// (another process may hold the file) nothing is truncated: the file keeps
    /// growing and the failure is noted once.
    fn rotate(&mut self) {
        let current = self.dir.join(LOG_FILE);
        if let Err(e) = std::fs::rename(&current, self.dir.join(OLD_LOG_FILE)) {
            self.last_failed_rotation = Some(Instant::now());
            if !self.rotate_failure_noted {
                self.rotate_failure_noted = true;
                let note = format!(
                    "{} [{}] log rotation failed, continuing in the current file: {e}\n",
                    timestamp(),
                    thread_name()
                );
                if self.file.write_all(note.as_bytes()).is_ok() {
                    self.size += note.len() as u64;
                }
            }
            return;
        }
        self.last_failed_rotation = None;
        if let Ok(file) = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&current)
        {
            self.file = file;
            self.size = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tagged_process_marks_every_thread_name() {
        assert_eq!(tagged(Some("settings"), "main"), "settings main");
        assert_eq!(tagged(None, "main"), "main");
    }

    /// Serialises tests that touch the global [`LOG`].
    static GLOBAL_LOG_LOCK: Mutex<()> = Mutex::new(());

    /// Releases the global log file (so a temp dir can be removed) even if an
    /// assert panics.
    struct ResetLog;
    impl Drop for ResetLog {
        fn drop(&mut self) {
            *LOG.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }

    #[test]
    fn init_is_idempotent() {
        let _serial = GLOBAL_LOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        // Declared after the temp dirs so it drops (and closes the file) first.
        let _reset = ResetLog;
        init(first.path());
        init(second.path());
        assert!(first.path().join(LOG_FILE).exists());
        assert!(!second.path().join(LOG_FILE).exists());
    }

    #[test]
    fn append_adds_lines_to_the_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(LOG_FILE), "earlier\n").unwrap();

        let mut log = LogFile::open(tmp.path(), 1024).unwrap();
        log.append("one\n");
        log.append("two\n");

        let text = std::fs::read_to_string(tmp.path().join(LOG_FILE)).unwrap();
        assert_eq!(text, "earlier\none\ntwo\n");
    }

    #[test]
    fn passing_the_cap_moves_the_file_aside_and_starts_a_new_one() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = LogFile::open(tmp.path(), 10).unwrap();
        log.owner = true;

        log.append("12345678\n");
        log.append("abc\n");

        let current = std::fs::read_to_string(tmp.path().join(LOG_FILE)).unwrap();
        let old = std::fs::read_to_string(tmp.path().join(OLD_LOG_FILE)).unwrap();
        assert_eq!(current, "abc\n");
        assert_eq!(old, "12345678\n");
    }

    #[test]
    fn a_non_owner_past_the_cap_appends_without_rotating() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = LogFile::open(tmp.path(), 10).unwrap();

        log.append("12345678\n");
        log.append("abc\n");

        let current = std::fs::read_to_string(tmp.path().join(LOG_FILE)).unwrap();
        assert_eq!(current, "12345678\nabc\n");
        assert!(!tmp.path().join(OLD_LOG_FILE).exists());
    }

    #[test]
    fn a_failed_rotation_still_writes_the_line_and_notes_the_failure_once() {
        let tmp = tempfile::tempdir().unwrap();
        // A directory at the target makes the rename fail.
        std::fs::create_dir(tmp.path().join(OLD_LOG_FILE)).unwrap();
        let mut log = LogFile::open(tmp.path(), 10).unwrap();
        log.owner = true;

        log.append("12345678\n");
        log.append("abc\n");
        log.append("def\n");

        let current = std::fs::read_to_string(tmp.path().join(LOG_FILE)).unwrap();
        assert!(current.starts_with("12345678\n"), "{current}");
        assert!(current.contains("abc\n") && current.contains("def\n"));
        assert_eq!(
            current.matches("log rotation failed").count(),
            1,
            "{current}"
        );
    }

    #[test]
    fn warn_and_error_are_forwarded_from_every_crate() {
        for level in [log::Level::Warn, log::Level::Error] {
            assert!(should_forward(level, "winit::platform_impl"));
            assert!(should_forward(level, "gameframe"));
        }
    }

    #[test]
    fn info_is_forwarded_only_from_the_gpu_crates() {
        assert!(should_forward(log::Level::Info, "egui_wgpu"));
        assert!(should_forward(log::Level::Info, "egui_wgpu::winit"));
        assert!(should_forward(log::Level::Info, "wgpu_core::instance"));
        assert!(should_forward(log::Level::Info, "wgpu_hal::dx12::adapter"));
        assert!(!should_forward(log::Level::Info, "winit::window"));
        assert!(!should_forward(log::Level::Info, "eframe::native"));
        assert!(!should_forward(log::Level::Info, "wgpu_hal_extra"));
    }

    #[test]
    fn debug_and_trace_are_never_forwarded() {
        for level in [log::Level::Debug, log::Level::Trace] {
            assert!(!should_forward(level, "wgpu_hal::dx12"));
            assert!(!should_forward(level, "egui_wgpu"));
        }
    }

    #[test]
    fn repeats_within_the_window_are_suppressed_then_counted() {
        let mut f = RepeatFilter::default();
        let t0 = Instant::now();
        assert_eq!(f.check("a", t0), Repeat::Log(0));
        assert_eq!(f.check("a", t0 + Duration::from_secs(1)), Repeat::Suppress);
        assert_eq!(f.check("a", t0 + Duration::from_secs(59)), Repeat::Suppress);
        assert_eq!(f.check("b", t0 + Duration::from_secs(30)), Repeat::Log(0));
        assert_eq!(f.check("a", t0 + REPEAT_WINDOW), Repeat::Log(2));
        assert_eq!(f.check("a", t0 + REPEAT_WINDOW), Repeat::Suppress);
    }

    #[test]
    fn the_repeat_memory_is_bounded() {
        let mut f = RepeatFilter::default();
        let t0 = Instant::now();
        for i in 0..=MAX_TRACKED {
            f.check(&i.to_string(), t0);
        }
        assert!(f.seen.len() <= MAX_TRACKED);
    }

    #[test]
    fn evicting_a_record_with_suppressed_repeats_yields_its_count() {
        let mut f = RepeatFilter::default();
        let t0 = Instant::now();
        f.check("noisy", t0);
        f.check("noisy", t0);
        f.check("noisy", t0);
        for i in 0..MAX_TRACKED {
            f.check(&i.to_string(), t0);
        }
        assert_eq!(f.evicted, vec![("noisy".to_string(), 2)]);
        assert_eq!(f.take_pending(), vec!["noisy (repeated 2 more times)"]);
        assert!(f.take_pending().is_empty());
    }

    #[test]
    fn take_pending_yields_every_pending_count_and_clears_them() {
        let mut f = RepeatFilter::default();
        let t0 = Instant::now();
        for _ in 0..3 {
            f.check("a", t0);
        }
        f.check("b", t0);
        for _ in 0..2 {
            f.check("c", t0);
        }
        let mut lines = f.take_pending();
        lines.sort();
        assert_eq!(
            lines,
            vec!["a (repeated 2 more times)", "c (repeated 1 more times)"]
        );
        assert!(f.take_pending().is_empty());
        // The cleared count does not come back on the next window.
        assert_eq!(f.check("a", t0 + REPEAT_WINDOW), Repeat::Log(0));
    }

    #[test]
    fn a_failed_rotation_is_retried_only_after_the_retry_interval() {
        let t0 = Instant::now();
        assert!(rotation_due(None, t0));
        assert!(!rotation_due(Some(t0), t0 + Duration::from_secs(1)));
        assert!(!rotation_due(
            Some(t0),
            t0 + ROTATE_RETRY - Duration::from_millis(1)
        ));
        assert!(rotation_due(Some(t0), t0 + ROTATE_RETRY));
    }

    #[test]
    fn claim_rotation_marks_an_open_log_and_does_nothing_before_init() {
        let _serial = GLOBAL_LOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        // Declared after the temp dir so it drops (and closes the file) first.
        let _reset = ResetLog;
        *LOG.lock().unwrap_or_else(|e| e.into_inner()) = None;

        claim_rotation(); // before init: no file to mark, no panic
        assert!(LOG.lock().unwrap_or_else(|e| e.into_inner()).is_none());

        *LOG.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(LogFile::open(tmp.path(), 1024).unwrap());
        assert!(
            !LOG.lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .unwrap()
                .owner
        );
        claim_rotation();
        let owner = LOG
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .unwrap()
            .owner;
        assert!(owner);
    }

    #[test]
    fn a_rotation_failure_starts_the_retry_throttle_and_success_clears_it() {
        let tmp = tempfile::tempdir().unwrap();
        // A directory at the target makes the rename fail.
        std::fs::create_dir(tmp.path().join(OLD_LOG_FILE)).unwrap();
        let mut log = LogFile::open(tmp.path(), 10).unwrap();
        log.owner = true;

        log.append("12345678\n");
        assert!(log.last_failed_rotation.is_none());
        log.append("abc\n"); // over the cap: rotation fails
        assert!(log.last_failed_rotation.is_some());

        // Within the retry interval the rename is not attempted again, and a
        // stale failure time (past the interval) allows a retry that succeeds.
        std::fs::remove_dir(tmp.path().join(OLD_LOG_FILE)).unwrap();
        log.append("def\n");
        assert!(!tmp.path().join(OLD_LOG_FILE).exists());
        log.last_failed_rotation = Some(Instant::now() - ROTATE_RETRY);
        log.append("ghi\n");
        assert!(tmp.path().join(OLD_LOG_FILE).exists());
        assert!(log.last_failed_rotation.is_none());
    }

    #[test]
    fn flush_writes_evicted_and_pending_count_lines_to_the_log() {
        let _serial = GLOBAL_LOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let _reset = ResetLog;
        *LOG.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(LogFile::open(tmp.path(), 1 << 20).unwrap());
        let mut f = RepeatFilter::default();
        let t0 = Instant::now();
        for _ in 0..3 {
            f.check("evictme", t0);
        }
        for i in 0..MAX_TRACKED {
            f.check(&i.to_string(), t0);
        }
        for _ in 0..2 {
            f.check("pending", t0);
        }
        *REPEATS.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);

        log::Log::flush(&LogBridge);

        let text = std::fs::read_to_string(tmp.path().join(LOG_FILE)).unwrap();
        assert!(text.contains("evictme (repeated 2 more times)"), "{text}");
        assert!(text.contains("pending (repeated 1 more times)"), "{text}");
        // Flushing again writes nothing new.
        log::Log::flush(&LogBridge);
        let again = std::fs::read_to_string(tmp.path().join(LOG_FILE)).unwrap();
        // (Other tests' `log!` lines may land in the file meanwhile; count ours.)
        assert_eq!(again.matches("evictme (repeated").count(), 1, "{again}");
        assert_eq!(again.matches("pending (repeated").count(), 1, "{again}");
        *REPEATS.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    #[test]
    fn timestamp_has_a_fixed_shape() {
        let t = timestamp();
        assert_eq!(t.len(), "2026-01-02 03:04:05.006".len(), "{t}");
        assert_eq!(&t[4..5], "-");
        assert_eq!(&t[10..11], " ");
    }
}
