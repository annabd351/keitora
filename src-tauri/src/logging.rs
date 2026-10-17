//! `logging` — the app logger: `env_logger` to stderr, teed to a rotating file.
//!
//! Release builds on Windows have no console, so stderr alone leaves nothing to
//! read after a crash. [`init`] keeps `env_logger`'s filtering and stderr output
//! (`RUST_LOG`, default `info`) and also appends every record it lets through to
//! `<KEITORA_HOME>/logs/keitora.log`. Records on the [`LIFECYCLE_TARGET`] target
//! always reach the file, whatever `RUST_LOG` says.
//!
//! The file sink is best-effort: each record is one unbuffered `write_all` of a
//! whole line (a record is complete or absent, nothing sits in a buffer when the
//! process aborts), and every I/O error is swallowed. Rotation is size-based:
//! `keitora.log` -> `keitora.log.1` (newest) ... `keitora.log.4` (oldest).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use log::{Level, LevelFilter, Log, Metadata, Record};

pub const LOG_FILE_NAME: &str = "keitora.log";
pub const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
pub const KEEP_ROTATED: usize = 4;
/// `log` target used by lifecycle records (always written to the file).
pub const LIFECYCLE_TARGET: &str = "lifecycle";

/// After a failed rotation, keep appending and retry only once this many more
/// bytes have been written, so a stuck rename cannot cost a syscall per line.
const ROTATE_RETRY_BYTES: u64 = 256 * 1024;

/// `<KEITORA_HOME>/logs`. Honouring `KEITORA_HOME` keeps isolated dev instances'
/// logs apart from the real app's.
pub fn log_dir() -> PathBuf {
    crate::paths::keitora_home().join("logs")
}

/// One file line: `{ts} {LEVEL:<5} [{target}] {message}\n`. Embedded `\r` / `\n`
/// in the message become the two characters `\r` / `\n`, so a record is always
/// exactly one line.
pub fn format_line(ts: &str, level: Level, target: &str, message: &str) -> String {
    let message = message.replace('\r', "\\r").replace('\n', "\\n");
    format!("{ts} {:<5} [{target}] {message}\n", level.as_str())
}

/// Append-only log file with size-based rotation.
pub(crate) struct RotatingFile {
    dir: PathBuf,
    file: File,
    written: u64,
    max_bytes: u64,
    keep: usize,
    /// `written` must reach this before rotation is attempted again (0 = no
    /// failed rotation pending).
    next_retry_at: u64,
}

impl RotatingFile {
    /// Open (creating `dir`) the current log file for append. A file already at or
    /// over the cap is rotated first.
    pub(crate) fn open(dir: &Path, max_bytes: u64, keep: usize) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let file = open_append(&dir.join(LOG_FILE_NAME))?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        let mut rf = RotatingFile {
            dir: dir.to_path_buf(),
            file,
            written,
            max_bytes,
            keep,
            next_retry_at: 0,
        };
        if rf.written >= rf.max_bytes {
            rf.rotate();
        }
        Ok(rf)
    }

    /// Append one whole line, rotating first when it would push the file past the
    /// cap.
    pub(crate) fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        let len = line.len() as u64;
        if self.written > 0
            && self.written.saturating_add(len) > self.max_bytes
            && self.written >= self.next_retry_at
        {
            self.rotate();
        }
        self.file.write_all(line.as_bytes())?;
        self.written = self.written.saturating_add(len);
        Ok(())
    }

    fn rotated_path(&self, n: usize) -> PathBuf {
        self.dir.join(format!("{LOG_FILE_NAME}.{n}"))
    }

    fn rotate(&mut self) {
        match self.try_rotate() {
            Ok(()) => self.next_retry_at = 0,
            Err(_) => self.next_retry_at = self.written.saturating_add(ROTATE_RETRY_BYTES),
        }
    }

    fn try_rotate(&mut self) -> std::io::Result<()> {
        let current = self.dir.join(LOG_FILE_NAME);
        if self.keep == 0 {
            std::fs::remove_file(&current)?;
        } else {
            // Shift the older files up one slot (missing ones are fine), then move
            // the current file to `.1`; only that last rename decides success.
            let _ = std::fs::remove_file(self.rotated_path(self.keep));
            for n in (1..self.keep).rev() {
                let _ = std::fs::rename(self.rotated_path(n), self.rotated_path(n + 1));
            }
            std::fs::rename(&current, self.rotated_path(1))?;
        }
        self.file = open_append(&current)?;
        self.written = self.file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(())
    }
}

fn open_append(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

/// `env_logger` for stderr + filtering, teed to the rotating file.
struct KeitoraLogger {
    inner: env_logger::Logger,
    file: Option<Mutex<RotatingFile>>,
}

impl KeitoraLogger {
    fn write_file(&self, record: &Record) {
        let Some(file) = &self.file else {
            return;
        };
        let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let line = format_line(
            &ts,
            record.level(),
            record.target(),
            &record.args().to_string(),
        );
        // A poisoned lock means another thread panicked mid-write: skip this record.
        if let Ok(mut f) = file.lock() {
            let _ = f.write_line(&line);
        }
    }
}

fn is_lifecycle(target: &str, level: Level) -> bool {
    target == LIFECYCLE_TARGET && level <= Level::Info
}

impl Log for KeitoraLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.inner.enabled(metadata) || is_lifecycle(metadata.target(), metadata.level())
    }

    fn log(&self, record: &Record) {
        let matches = self.inner.matches(record);
        if matches {
            self.inner.log(record);
        }
        if matches || is_lifecycle(record.target(), record.level()) {
            self.write_file(record);
        }
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

/// Install the app logger and the panic hook. Never fails: if the log file cannot
/// be opened the app logs to stderr only.
pub fn init() {
    let inner =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).build();
    let file = RotatingFile::open(&log_dir(), MAX_LOG_BYTES, KEEP_ROTATED)
        .ok()
        .map(Mutex::new);
    let file_ok = file.is_some();
    // At least Info, so lifecycle records pass the global gate under RUST_LOG=warn.
    log::set_max_level(inner.filter().max(LevelFilter::Info));
    let _ = log::set_boxed_logger(Box::new(KeitoraLogger { inner, file }));

    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!("panic: {info}");
        prev(info);
    }));

    log::info!(
        "keitora start version={} pid={} log_dir={}",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        log_dir().display()
    );
    if !file_ok {
        log::warn!("file logging disabled");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(dir: &Path, name: &str) -> String {
        std::fs::read_to_string(dir.join(name)).unwrap_or_default()
    }

    /// Highest `line N` number in a file's contents.
    fn last_line_no(text: &str) -> u32 {
        text.lines()
            .filter_map(|l| l.strip_prefix("line ")?.trim().parse().ok())
            .max()
            .expect("file has numbered lines")
    }

    #[test]
    fn format_line_golden() {
        assert_eq!(
            format_line(
                "2026-10-05T08:03:19.123Z",
                Level::Info,
                "lifecycle",
                "event=x"
            ),
            "2026-10-05T08:03:19.123Z INFO  [lifecycle] event=x\n"
        );
        assert_eq!(
            format_line("t", Level::Error, "a::b", "boom"),
            "t ERROR [a::b] boom\n"
        );
    }

    #[test]
    fn format_line_escapes_embedded_newlines() {
        let line = format_line("t", Level::Warn, "x", "one\ntwo\r\nthree");
        assert_eq!(line, "t WARN  [x] one\\ntwo\\r\\nthree\n");
        assert_eq!(line.matches('\n').count(), 1);
        assert!(line.ends_with('\n'));
    }

    #[test]
    fn rotating_file_rotates_and_keeps_bounded_history() {
        let tmp = tempfile::tempdir().unwrap();
        let mut f = RotatingFile::open(tmp.path(), 64, 2).unwrap();
        for i in 0..40 {
            f.write_line(&format!("line {i:02}\n")).unwrap();
        }
        drop(f);
        let dir = tmp.path();
        assert!(dir.join("keitora.log").exists());
        assert!(dir.join("keitora.log.1").exists());
        assert!(dir.join("keitora.log.2").exists());
        assert!(!dir.join("keitora.log.3").exists());
        let current = read(dir, "keitora.log");
        assert!(current.contains("line 39"), "newest line in current file");
        let one = last_line_no(&read(dir, "keitora.log.1"));
        let two = last_line_no(&read(dir, "keitora.log.2"));
        assert!(last_line_no(&current) > one);
        assert!(one > two, ".1 is newer than .2");
        for name in ["keitora.log", "keitora.log.1", "keitora.log.2"] {
            assert!(std::fs::metadata(dir.join(name)).unwrap().len() <= 64);
        }
    }

    #[test]
    fn rotating_file_rotates_oversized_file_at_open() {
        let tmp = tempfile::tempdir().unwrap();
        let old = "x".repeat(100);
        std::fs::write(tmp.path().join("keitora.log"), &old).unwrap();
        let f = RotatingFile::open(tmp.path(), 64, 2).unwrap();
        drop(f);
        assert_eq!(read(tmp.path(), "keitora.log.1"), old);
        assert_eq!(read(tmp.path(), "keitora.log"), "");
    }

    #[test]
    fn rotating_file_open_fails_under_a_regular_file() {
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("not-a-dir");
        std::fs::write(&blocker, "x").unwrap();
        assert!(RotatingFile::open(&blocker.join("logs"), 64, 2).is_err());
    }

    #[test]
    fn logger_without_file_logs_without_panicking() {
        let logger = KeitoraLogger {
            inner: env_logger::Builder::new()
                .filter_level(LevelFilter::Off)
                .build(),
            file: None,
        };
        logger.log(
            &Record::builder()
                .args(format_args!("no file"))
                .level(Level::Info)
                .target(LIFECYCLE_TARGET)
                .build(),
        );
        logger.flush();
    }

    #[test]
    fn lifecycle_target_reaches_file_despite_inner_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let logger = KeitoraLogger {
            inner: env_logger::Builder::new()
                .filter_level(LevelFilter::Warn)
                .build(),
            file: Some(Mutex::new(
                RotatingFile::open(tmp.path(), MAX_LOG_BYTES, KEEP_ROTATED).unwrap(),
            )),
        };
        logger.log(
            &Record::builder()
                .args(format_args!("event=kept"))
                .level(Level::Info)
                .target(LIFECYCLE_TARGET)
                .build(),
        );
        logger.log(
            &Record::builder()
                .args(format_args!("plain info dropped"))
                .level(Level::Info)
                .target("keitora_lib::host")
                .build(),
        );
        drop(logger);
        let text = read(tmp.path(), "keitora.log");
        assert!(text.contains("INFO  [lifecycle] event=kept"), "{text}");
        assert!(!text.contains("plain info dropped"), "{text}");
        assert_eq!(text.lines().count(), 1);
    }
}
