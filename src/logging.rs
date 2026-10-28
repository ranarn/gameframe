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
//! from dependencies (egui-wgpu, wgpu, winit, ...) land in the same file:
//! `warn`/`error` from every crate, plus `info` from the GPU crates, whose
//! adapter/backend/alpha-mode lines show what the zone-editor overlay got.
//!
//! Before [`init`] runs (and in tests) lines go to stderr only.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
/// costs the file output; logging to stderr keeps working.
pub fn init(dir: &Path) {
    install_log_bridge();
    match LogFile::open(dir, MAX_LOG_BYTES) {
        Ok(file) => *LOG.lock().unwrap_or_else(|e| e.into_inner()) = Some(file),
        Err(e) => eprintln!("could not open the log file in {}: {e}", dir.display()),
    }
}

/// Crates whose `info` records are forwarded (see [`should_forward`]).
const INFO_TARGET_PREFIXES: [&str; 3] = ["egui_wgpu", "wgpu_core", "wgpu_hal"];

/// Whether a `log` crate record is written to the log file: `warn` and
/// `error` from every crate, `info` only from the GPU crates (adapter name,
/// backend, chosen alpha mode); `debug`/`trace` never, so there is no
/// per-frame noise. A target matches a prefix as a whole path segment
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

/// `log::Log` implementation that writes through [`write`].
struct LogBridge;

impl log::Log for LogBridge {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        should_forward(metadata.level(), metadata.target())
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            write(format_args!(
                "{} {}: {}",
                record.level(),
                record.target(),
                record.args()
            ));
        }
    }

    fn flush(&self) {}
}

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

fn thread_name() -> String {
    let thread = std::thread::current();
    thread
        .name()
        .map(str::to_string)
        .unwrap_or_else(|| format!("{:?}", thread.id()))
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
        })
    }

    /// Appends `line`, rotating first if it would pass the cap. A failed
    /// write is dropped: there is nowhere left to report it.
    fn append(&mut self, line: &str) {
        if self.size > 0 && self.size + line.len() as u64 > self.max_bytes {
            self.rotate();
        }
        if self.file.write_all(line.as_bytes()).is_ok() {
            self.size += line.len() as u64;
        }
    }

    fn rotate(&mut self) {
        let current = self.dir.join(LOG_FILE);
        let _ = std::fs::rename(&current, self.dir.join(OLD_LOG_FILE));
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

        log.append("12345678\n");
        log.append("abc\n");

        let current = std::fs::read_to_string(tmp.path().join(LOG_FILE)).unwrap();
        let old = std::fs::read_to_string(tmp.path().join(OLD_LOG_FILE)).unwrap();
        assert_eq!(current, "abc\n");
        assert_eq!(old, "12345678\n");
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
    fn timestamp_has_a_fixed_shape() {
        let t = timestamp();
        assert_eq!(t.len(), "2026-01-02 03:04:05.006".len(), "{t}");
        assert_eq!(&t[4..5], "-");
        assert_eq!(&t[10..11], " ");
    }
}
